//! Shared per-credential quota snapshots. No network access occurs on reads.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use ego_core::quota::{RateLimits, RateLimitsReader};
use fd_lock::RwLock;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct QuotaStore {
    home: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct QuotaState {
    pub rate_limits: Vec<RateLimits>,
    #[serde(default)]
    pub next_read: std::collections::BTreeMap<String, u64>,
    #[serde(default)]
    pub read_issues: std::collections::BTreeMap<String, crate::auth_usage::UsageIssue>,
}

impl QuotaStore {
    pub fn new(home: impl AsRef<Path>) -> Self {
        Self {
            home: home.as_ref().to_owned(),
        }
    }

    pub(crate) fn lock(&self) -> std::io::Result<RwLock<File>> {
        fs::create_dir_all(&self.home)?;
        Ok(RwLock::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(self.home.join("quota.lock"))?,
        ))
    }

    pub(crate) fn load(&self) -> std::io::Result<QuotaState> {
        match fs::read(self.home.join("quota.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(QuotaState::default()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn save(&self, state: &QuotaState) -> std::io::Result<()> {
        let path = self.home.join("quota.json.tmp");
        fs::write(
            &path,
            serde_json::to_vec(state).map_err(std::io::Error::other)?,
        )?;
        fs::rename(path, self.home.join("quota.json"))
    }

    pub(crate) fn replace(state: &mut QuotaState, value: RateLimits) {
        let old = state
            .rate_limits
            .iter()
            .position(|old| old.provider == value.provider && old.credential == value.credential);
        if let Some(index) = old {
            if state.rate_limits[index].observed_at > value.observed_at {
                return;
            }
            state.rate_limits[index] = value;
        } else {
            state.rate_limits.push(value);
        }
    }

    pub fn observe(&self, value: RateLimits) -> std::io::Result<()> {
        let mut lock = self.lock()?;
        let _guard = lock.write()?;
        let mut state = self.load()?;
        Self::replace(&mut state, value);
        self.save(&state)
    }
}

impl RateLimitsReader for QuotaStore {
    fn rate_limits(&self) -> Vec<RateLimits> {
        // Atomic rename means readers see the complete old or complete new document.
        self.load()
            .map(|state| state.rate_limits)
            .unwrap_or_default()
    }
}

/// The publication reader excludes snapshots of credentials no longer selected.
#[derive(Debug)]
pub struct CurrentQuotaReader(pub PathBuf);

impl RateLimitsReader for CurrentQuotaReader {
    fn rate_limits(&self) -> Vec<RateLimits> {
        let Ok(entries) = crate::auth::AuthStore::new(&self.0).read_observed() else {
            return Vec::new();
        };
        // Unreadable configuration cannot authorize account attribution.
        let config = crate::auth::connection_layer_at(&self.0).ok();
        let snapshots = QuotaStore::new(&self.0).rate_limits();
        entries
            .into_iter()
            .flat_map(|(key, entry)| {
                let crate::auth::AuthEntry::OAuth { access, .. } = entry else {
                    return Vec::new();
                };
                let (route, provider, connection) =
                    match crate::auth::connection_id_from_storage_key(&key) {
                        Some(id) => {
                            let Some(connection) = config
                                .as_ref()
                                .and_then(|config| config.connections.get(id))
                            else {
                                return Vec::new();
                            };
                            if connection.access != ego_core::config::ConnectionAccess::Oauth {
                                return Vec::new();
                            }
                            (
                                crate::auth::connection_route(connection),
                                connection.vendor.clone(),
                                Some(id.to_owned()),
                            )
                        }
                        None => return Vec::new(),
                    };
                let Ok(secret) = std::str::from_utf8(access.expose_secret_bytes()) else {
                    return Vec::new();
                };
                let Ok(auth) = ego_ai::oauth::provider_auth(&route, secret) else {
                    return Vec::new();
                };
                let credential = ego_ai::usage_limits::credential_fingerprint(&auth.headers());
                snapshots
                    .iter()
                    .filter(|quota| quota.provider == route && quota.credential == credential)
                    .map(|quota| {
                        let mut quota = quota.clone();
                        quota.provider = provider.clone();
                        quota.connection = connection.clone();
                        quota
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

pub(crate) fn current() -> Vec<RateLimits> {
    crate::config::ego_home()
        .map(|home| CurrentQuotaReader(home).rate_limits())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Catches: namespaced storage IDs are passed to vendor auth and named passive quota disappears.
    #[test]
    fn named_connections_passive_quota_preserves_vendor_and_account_identity() {
        use crate::auth::{AuthEntry, AuthStore, connection_storage_key};
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), "[connections.anthropic]\nname='Same label'\nvendor='anthropic'\naccess='oauth'\n[connections.account_b]\nname='Same label'\nvendor='anthropic'\naccess='oauth'\n[connections.account_a]\nname='Same label'\nvendor='anthropic'\naccess='oauth'\n[connections.key_account]\nname='Key'\nvendor='anthropic'\naccess='key'\nkey_env='UNUSED_API_KEY'\n").unwrap();
        let auth = AuthStore::new(home.path());
        let store = QuotaStore::new(home.path());
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../ego-ai/tests/fixtures/subscription_quota/contract.json"
        ))
        .unwrap();
        for (key, token, used) in [
            ("anthropic".to_owned(), "sk-ant-oat-test-legacy", 11),
            (connection_storage_key("anthropic"), "sk-ant-oat-test-a", 22),
            (connection_storage_key("account_b"), "sk-ant-oat-test-b", 33),
            (
                connection_storage_key("account_a"),
                "sk-ant-oat-test-account-a",
                66,
            ),
            (
                connection_storage_key("orphan"),
                "sk-ant-oat-test-orphan",
                44,
            ),
            (
                connection_storage_key("key_account"),
                "sk-ant-oat-test-key",
                55,
            ),
        ] {
            auth.put(
                &key,
                AuthEntry::OAuth {
                    access: token.to_owned().into(),
                    refresh: None,
                    expires: None,
                    scope_binding: None,
                },
            )
            .unwrap();
            let mut quota: RateLimits =
                serde_json::from_value(fixture["rate_limits"][0].clone()).unwrap();
            let headers = ego_ai::oauth::provider_auth("anthropic", token)
                .unwrap()
                .headers();
            quota.credential = ego_ai::usage_limits::credential_fingerprint(&headers);
            quota.windows[0].used_percentage = used.into();
            store.observe(quota).unwrap();
        }
        let quotas = CurrentQuotaReader(home.path().into()).rate_limits();
        let raw = serde_json::to_string(&quotas).unwrap();
        println!("PASSIVE_QUOTA_CONTRACT_FIXTURE={raw}");
        let output: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let rows = output.as_array().unwrap();
        assert_eq!(
            rows.len(),
            3,
            "only three configured named OAuth accounts may publish quota: {output}"
        );
        for (connection, used) in [
            (Some("anthropic"), 22),
            (Some("account_b"), 33),
            (Some("account_a"), 66),
        ] {
            let row = rows
                .iter()
                .find(|row| row["connection"].as_str() == connection)
                .unwrap();
            assert_eq!(row["provider"], "anthropic");
            assert_eq!(row["windows"][0]["used_percentage"], used);
        }
        // Catches: attribution without validated named-account configuration.
        for config in [None, Some("[connections.account_b")] {
            let path = home.path().join("config.toml");
            if let Some(text) = config {
                std::fs::write(path, text).unwrap();
            } else {
                std::fs::remove_file(path).unwrap();
            }
            let output =
                serde_json::to_value(CurrentQuotaReader(home.path().into()).rate_limits()).unwrap();
            assert!(
                output.as_array().unwrap().is_empty(),
                "{config:?}: {output}"
            );
        }
    }

    // Catches: an older response overwrites the newest snapshot shared by processes.
    #[test]
    fn quota_store_keeps_newest_per_credential() {
        let home = tempfile::tempdir().unwrap();
        let first = QuotaStore::new(home.path());
        let second = QuotaStore::new(home.path());
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../ego-ai/tests/fixtures/subscription_quota/contract.json"
        ))
        .unwrap();
        let mut value: RateLimits =
            serde_json::from_value(fixture["rate_limits"][0].clone()).unwrap();
        first.observe(value.clone()).unwrap();
        value.observed_at -= 1;
        second.observe(value).unwrap();
        assert_eq!(first.rate_limits(), second.rate_limits());
        assert_eq!(first.rate_limits()[0].observed_at, 1794458253279);
    }
}
