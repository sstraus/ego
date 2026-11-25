//! Non-generating quota reads with shared cadence and no OAuth refresh.

use std::path::Path;
use std::time::Duration;

use ego_ai::usage_limits::{SubscriptionUsageResponse, credential_fingerprint};
use ego_core::event::time::Clock;
use ego_core::quota::{QuotaCredits, QuotaSource, RateLimitWindow, RateLimits, window_name};
use ego_core::turn::CancelToken;
use ego_tools::clock::SystemClock;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::auth::{AuthEntry, AuthStore};
use crate::quota::QuotaStore;

const CADENCE_MS: u64 = 300_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UsageIssue {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection: Option<String>,
    state: String,
    message: String,
    retry_at: Option<u64>,
}

fn issue(provider: &str, state: &str, message: &str, retry_at: Option<u64>) -> UsageIssue {
    UsageIssue {
        provider: Some(provider.into()),
        connection: None,
        state: state.into(),
        message: message.into(),
        retry_at,
    }
}

#[derive(Serialize)]
struct UsageOutput {
    rate_limits: Vec<RateLimits>,
    errors: Vec<UsageIssue>,
}

trait UsageFetcher {
    fn fetch<'a>(
        &'a self,
        provider: &'a str,
        access: &'a str,
    ) -> BoxFuture<'a, Result<SubscriptionUsageResponse, String>>;
}

struct NetworkFetcher(CancelToken);
impl UsageFetcher for NetworkFetcher {
    fn fetch<'a>(
        &'a self,
        provider: &'a str,
        access: &'a str,
    ) -> BoxFuture<'a, Result<SubscriptionUsageResponse, String>> {
        Box::pin(async move {
            if crate::config::user_effective()
                .map_err(|e| e.to_string())?
                .network
                == ego_core::config::Network::Off
            {
                return Err("network is disabled".into());
            }
            let scope = crate::provider_http::CommandHttpScope::online(self.0.clone()).await?;
            ego_ai::usage_limits::subscription_usage(provider, access, scope.http()).await
        })
    }
}

pub(crate) async fn execute(
    home: &Path,
    provider: Option<&str>,
    vendor_filter: Option<&str>,
    json: bool,
    cancel: &CancelToken,
) -> Result<String, String> {
    let entries = AuthStore::new(home)
        .read_observed()
        .map_err(|e| e.to_string())?;
    let config = crate::auth::connection_layer_at(home).map_err(|error| error.to_string())?;
    let connections = &config.connections;
    let store = QuotaStore::new(home);
    let mut output = UsageOutput {
        rate_limits: Vec::new(),
        errors: Vec::new(),
    };
    let fetcher = NetworkFetcher(cancel.clone());
    let mut found = false;
    for (key, entry) in &entries {
        if provider.is_some_and(|selected| selected != key) {
            continue;
        }
        let Some(id) = crate::auth::connection_id_from_storage_key(key) else {
            continue;
        };
        let Some(connection) = connections.get(id) else {
            continue;
        };
        let vendor = connection.vendor.as_str();
        if connection.access != ego_core::config::ConnectionAccess::Oauth
            || vendor_filter.is_some_and(|filter| filter != vendor)
        {
            continue;
        }
        let route = crate::auth::connection_route(connection);
        found = true;
        let (mut quota, mut error) =
            read_one(&store, &route, entry, &SystemClock, &fetcher).await?;
        if let Some(quota) = quota.as_mut() {
            quota.provider = vendor.to_owned();
            quota.connection = Some(id.to_owned());
        }
        if let Some(error) = error.as_mut() {
            error.provider = Some(vendor.to_owned());
            error.connection = Some(id.to_owned());
            if error.state == "auth_invalid" {
                error.message = format!(
                    "{}; {}",
                    error.message,
                    crate::auth::renewal_hint(route.as_str())
                );
            }
        }
        output.rate_limits.extend(quota);
        output.errors.extend(error);
    }
    if !found {
        let named = provider.and_then(crate::auth::connection_id_from_storage_key);
        let key_connection = named
            .and_then(|id| connections.get(id))
            .is_some_and(|connection| connection.access == ego_core::config::ConnectionAccess::Key);
        let message = named
            .and_then(|id| {
                connections.get(id).map(|connection| {
                    if key_connection {
                        if connection.key_env.is_some() {
                            "subscription usage requires OAuth; this account uses an API-key environment source".into()
                        } else {
                            "subscription usage requires OAuth; this account uses a saved API key".into()
                        }
                    } else {
                        format!(
                            "no stored credential; {}",
                            crate::auth::renewal_hint(&crate::auth::connection_route(connection))
                        )
                    }
                })
            })
            .unwrap_or_else(|| "no stored credential; sign in again".into());
        let mut error = issue(
            provider.unwrap_or("all"),
            if key_connection {
                "unavailable"
            } else {
                "auth_invalid"
            },
            &message,
            None,
        );
        if let Some(id) = named {
            error.connection = Some(id.to_owned());
            error.provider = connections
                .get(id)
                .map(|connection| connection.vendor.clone());
        }
        output.errors.push(error);
    }
    if json {
        serde_json::to_string(&output)
            .map(|s| format!("{s}\n"))
            .map_err(|e| e.to_string())
    } else {
        let mut lines = Vec::new();
        for quota in output.rate_limits {
            for window in quota.windows {
                lines.push(format!(
                    "{} {}: {}% used",
                    quota.connection.as_deref().map_or_else(
                        || quota.provider.clone(),
                        |id| crate::auth::account_label(&config, &quota.provider, id)
                    ),
                    window.name,
                    window.used_percentage
                ));
            }
        }
        for error in output.errors {
            lines.push(format!(
                "{}: {}",
                match (error.connection.as_deref(), error.provider.as_deref()) {
                    (Some(id), Some(provider)) => crate::auth::account_label(&config, provider, id),
                    (_, provider) => provider.unwrap_or("all").to_owned(),
                },
                error.message
            ));
        }
        Ok(format!("{}\n", lines.join("\n")))
    }
}

async fn read_one(
    store: &QuotaStore,
    provider: &str,
    entry: &AuthEntry,
    clock: &dyn Clock,
    fetcher: &dyn UsageFetcher,
) -> Result<(Option<RateLimits>, Option<UsageIssue>), String> {
    if provider == ego_core::config::AZURE_OPENAI
        || matches!(
            entry,
            AuthEntry::OAuth {
                scope_binding: Some(_),
                ..
            }
        )
    {
        return Ok((
            None,
            Some(issue(
                provider,
                "unavailable",
                "subscription usage is unavailable for azure-openai",
                None,
            )),
        ));
    }
    let now = clock.now().as_millis();
    let AuthEntry::OAuth {
        access, expires, ..
    } = entry
    else {
        return Ok((
            None,
            Some(issue(
                provider,
                "unavailable",
                "subscription usage requires an OAuth credential",
                None,
            )),
        ));
    };
    // Expiry is checked before credential parsing, transport creation, or any request.
    if expires.is_some_and(|at| at <= now) {
        return Ok((
            None,
            Some(issue(
                provider,
                "auth_invalid",
                "access token expired; sign in again",
                None,
            )),
        ));
    }
    if !matches!(provider, "anthropic" | "openai-codex") {
        // DEFERRED (2026-11-12): xAI needs a recorded response with a current token.
        return Ok((
            None,
            Some(issue(
                provider,
                "unavailable",
                "no recorded subscription usage endpoint",
                None,
            )),
        ));
    }
    let access = std::str::from_utf8(access.expose_secret_bytes())
        .map_err(|_| "invalid credential encoding")?;
    let headers = ego_ai::oauth::provider_auth(provider, access)
        .map_err(|_| "invalid subscription credential")?
        .headers();
    let credential = credential_fingerprint(&headers);
    // Request cooldown follows the token, while quota identity may follow an account claim.
    let read_key = format!("{provider}:{:x}", Sha256::digest(access.as_bytes()));
    let previous = {
        let mut lock = store.lock().map_err(|e| e.to_string())?;
        let _guard = lock.write().map_err(|e| e.to_string())?;
        let mut state = store.load().map_err(|e| e.to_string())?;
        let cached = state
            .rate_limits
            .iter()
            .find(|q| q.provider == provider && q.credential == credential)
            .cloned();
        if cached
            .as_ref()
            .is_some_and(|q| now < q.observed_at.saturating_add(CADENCE_MS))
        {
            return Ok((cached, None));
        }
        if state.next_read.get(&read_key).is_some_and(|at| *at > now) {
            let error = state.read_issues.get(&read_key).cloned().or_else(|| {
                cached.is_none().then(|| {
                    issue(
                        provider,
                        "backoff_until",
                        "quota read reserved or cooling down",
                        state.next_read.get(&read_key).copied(),
                    )
                })
            });
            return Ok((cached, error));
        }
        // Reserve under the file lock, then release before awaiting the network. A cold
        // follower returns the prior value (or an explicit reservation), never a second read.
        state
            .next_read
            .insert(read_key.clone(), now.saturating_add(CADENCE_MS));
        state.read_issues.remove(&read_key);
        store.save(&state).map_err(|e| e.to_string())?;
        cached
    };
    let response =
        tokio::time::timeout(Duration::from_secs(15), fetcher.fetch(provider, access)).await;
    let observed_at = clock.now().as_millis();
    let mut next_at = observed_at.saturating_add(CADENCE_MS);
    let (mut quota, error) = match response {
        Ok(Ok(response)) if response.status == 200 => {
            match parse_usage(provider, &credential, &response.body, observed_at) {
                Ok(value) => (Some(value), None),
                Err(_) => (
                    previous,
                    Some(issue(
                        provider,
                        "unavailable",
                        "invalid subscription usage response",
                        Some(next_at),
                    )),
                ),
            }
        }
        Ok(Ok(response)) if response.status == 429 => {
            next_at = retry_at(response.retry_after.as_deref(), observed_at).max(next_at);
            (
                previous,
                Some(issue(
                    provider,
                    "backoff_until",
                    "subscription usage rate limited",
                    Some(next_at),
                )),
            )
        }
        Ok(Ok(response)) if matches!(response.status, 401 | 403) => {
            next_at = u64::MAX;
            (
                previous,
                Some(issue(
                    provider,
                    "auth_invalid",
                    "credential rejected; sign in again",
                    None,
                )),
            )
        }
        Ok(Ok(_)) => (
            previous,
            Some(issue(
                provider,
                "unavailable",
                "subscription usage endpoint unavailable",
                Some(next_at),
            )),
        ),
        _ => (
            previous,
            Some(issue(
                provider,
                "unavailable",
                "subscription usage network read failed",
                Some(next_at),
            )),
        ),
    };
    {
        let mut lock = store.lock().map_err(|e| e.to_string())?;
        let _guard = lock.write().map_err(|e| e.to_string())?;
        let mut state = store.load().map_err(|e| e.to_string())?;
        if error.is_none()
            && let Some(value) = &quota
        {
            QuotaStore::replace(&mut state, value.clone());
        }
        state.next_read.insert(read_key.clone(), next_at);
        if let Some(error) = &error {
            state.read_issues.insert(read_key, error.clone());
        }
        store.save(&state).map_err(|e| e.to_string())?;
        quota = state
            .rate_limits
            .iter()
            .find(|q| q.provider == provider && q.credential == credential)
            .cloned()
            .or(quota);
    }
    Ok((quota, error))
}

fn retry_at(value: Option<&str>, now: u64) -> u64 {
    value
        .and_then(|s| {
            s.parse::<u64>()
                .ok()
                .map(|seconds| now.saturating_add(seconds.saturating_mul(1000)))
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc2822(s)
                        .ok()
                        .and_then(|date| u64::try_from(date.timestamp_millis()).ok())
                })
        })
        .unwrap_or_else(|| now.saturating_add(CADENCE_MS))
}

fn percent(value: &Value) -> Option<serde_json::Number> {
    let number = value.as_f64()?;
    (number.is_finite() && number >= 0.0)
        .then(|| serde_json::Number::from_f64(number))
        .flatten()
}

fn reset(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        chrono::DateTime::parse_from_rfc3339(value.as_str()?)
            .ok()
            .and_then(|date| u64::try_from(date.timestamp()).ok())
    })
}

fn safe_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

fn parse_usage(
    provider: &str,
    credential: &str,
    body: &[u8],
    observed_at: u64,
) -> Result<RateLimits, String> {
    let body: Value = serde_json::from_slice(body).map_err(|_| "invalid usage JSON")?;
    let mut windows = Vec::new();
    let mut plan = None;
    let mut credits = None;
    match provider {
        "anthropic" => {
            for (name, value) in body.as_object().ok_or("invalid usage object")? {
                let seconds = if name == "five_hour" {
                    18000
                } else if name == "seven_day" || name.starts_with("seven_day_") {
                    604800
                } else {
                    continue;
                };
                if safe_label(name)
                    && let Some(used_percentage) = percent(&value["utilization"])
                {
                    windows.push(RateLimitWindow {
                        name: name.clone(),
                        used_percentage,
                        resets_at: reset(&value["resets_at"]),
                        window_seconds: Some(seconds),
                    });
                }
            }
        }
        "openai-codex" => {
            for (scope, limit) in [
                ("", &body["rate_limit"]),
                ("code_review", &body["code_review_rate_limit"]),
            ] {
                for part in ["primary", "secondary"] {
                    let value = &limit[format!("{part}_window")];
                    let Some(seconds) = value["limit_window_seconds"].as_u64().filter(|v| *v > 0)
                    else {
                        continue;
                    };
                    if let Some(used_percentage) = percent(&value["used_percent"]) {
                        let mut name = window_name(seconds, part);
                        if !scope.is_empty() {
                            name = format!("{scope}_{name}");
                        }
                        if windows.iter().any(|w: &RateLimitWindow| w.name == name) {
                            name = format!("{name}_{part}");
                        }
                        windows.push(RateLimitWindow {
                            name,
                            used_percentage,
                            resets_at: reset(&value["reset_at"]),
                            window_seconds: Some(seconds),
                        });
                    }
                }
            }
            plan = body["plan_type"]
                .as_str()
                .filter(|s| safe_label(s))
                .map(str::to_owned);
            let value = &body["credits"];
            if let (Some(has_credits), Some(unlimited)) =
                (value["has_credits"].as_bool(), value["unlimited"].as_bool())
            {
                let balance = value["balance"]
                    .as_str()
                    .filter(|s| {
                        s.len() <= 64 && s.parse::<f64>().is_ok_and(|v| v.is_finite() && v >= 0.0)
                    })
                    .map(str::to_owned);
                credits = Some(QuotaCredits {
                    has_credits,
                    unlimited,
                    balance,
                });
            }
        }
        _ => return Err("no recorded usage parser".into()),
    }
    // No provider field is copied wholesale into output; identities and unknown fields stay private.
    if windows.is_empty() && credits.is_none() {
        return Err("usage response has no quota fields".into());
    }
    Ok(RateLimits {
        provider: provider.into(),
        connection: None,
        credential: credential.into(),
        windows,
        plan,
        credits,
        observed_at,
        source: QuotaSource::Active,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::event::time::Timestamp;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    struct TestClock(AtomicU64);
    impl Clock for TestClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_millis(self.0.load(Ordering::SeqCst))
        }
    }
    struct FixtureFetcher {
        calls: AtomicUsize,
        status: u16,
        retry: Option<String>,
    }
    impl UsageFetcher for FixtureFetcher {
        fn fetch<'a>(
            &'a self,
            _provider: &'a str,
            _access: &'a str,
        ) -> BoxFuture<'a, Result<SubscriptionUsageResponse, String>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok(SubscriptionUsageResponse {
                    status: self.status,
                    retry_after: self.retry.clone(),
                    body: serde_json::to_vec(&fixture("anthropic")["body"]).unwrap(),
                })
            })
        }
    }
    fn fixture(provider: &str) -> Value {
        serde_json::from_str(match provider {
            "anthropic" => {
                include_str!("../../ego-ai/tests/fixtures/subscription_quota/anthropic-usage.json")
            }
            _ => include_str!(
                "../../ego-ai/tests/fixtures/subscription_quota/openai-codex-usage.json"
            ),
        })
        .unwrap()
    }
    fn entry(expires: u64) -> AuthEntry {
        AuthEntry::OAuth {
            access: "sk-ant-oat-test-only".to_string().into(),
            refresh: Some("must-not-refresh".to_string().into()),
            expires: Some(expires),
            scope_binding: None,
        }
    }

    // Catches: each process or concurrent cold reader spends its own network request.
    #[tokio::test]
    async fn auth_usage_shares_one_read_per_credential() {
        let home = tempfile::tempdir().unwrap();
        let a = QuotaStore::new(home.path());
        let b = QuotaStore::new(home.path());
        let clock = TestClock(AtomicU64::new(1_000));
        let entry = entry(1_000_000);
        let fetcher = FixtureFetcher {
            calls: AtomicUsize::new(0),
            status: 200,
            retry: None,
        };
        let (first, second) = futures::join!(
            read_one(&a, "anthropic", &entry, &clock, &fetcher),
            read_one(&b, "anthropic", &entry, &clock, &fetcher)
        );
        assert!(first.unwrap().0.is_some());
        let second = second.unwrap();
        assert!(second.0.is_some() || second.1.is_some());
        let shared = read_one(&b, "anthropic", &entry, &clock, &fetcher)
            .await
            .unwrap();
        assert_eq!(
            shared.0.unwrap().windows[0].used_percentage.as_f64(),
            Some(82.0)
        );
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
        clock.0.store(301_001, Ordering::SeqCst);
        read_one(&b, "anthropic", &entry, &clock, &fetcher)
            .await
            .unwrap();
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2);
    }

    // Catches: an expired copied login enters the network or refresh path before validation.
    #[tokio::test]
    async fn expired_token_is_reported_without_a_request() {
        let home = tempfile::tempdir().unwrap();
        let store = QuotaStore::new(home.path());
        let clock = TestClock(AtomicU64::new(1000));
        let fetcher = FixtureFetcher {
            calls: AtomicUsize::new(0),
            status: 200,
            retry: None,
        };
        for expiry in [0, 999, 1000] {
            let (quota, error) = read_one(&store, "anthropic", &entry(expiry), &clock, &fetcher)
                .await
                .unwrap();
            assert!(quota.is_none());
            assert_eq!(error.as_ref().unwrap().state, "auth_invalid");
            assert!(error.unwrap().message.contains("sign in again"));
        }
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
        assert!(!home.path().join("quota.json").exists());
    }

    // Catches: fresh passive values still trigger idle polling, or active reads use another store.
    #[tokio::test]
    async fn fresh_passive_quota_suppresses_idle_read() {
        let home = tempfile::tempdir().unwrap();
        let store = QuotaStore::new(home.path());
        let entry = entry(1_000_000);
        let headers = ego_ai::oauth::provider_auth("anthropic", "sk-ant-oat-test-only")
            .unwrap()
            .headers();
        let mut value = parse_usage(
            "anthropic",
            &credential_fingerprint(&headers),
            &serde_json::to_vec(&fixture("anthropic")["body"]).unwrap(),
            1000,
        )
        .unwrap();
        value.source = QuotaSource::Passive;
        store.observe(value.clone()).unwrap();
        let clock = TestClock(AtomicU64::new(2000));
        let fetcher = FixtureFetcher {
            calls: AtomicUsize::new(0),
            status: 200,
            retry: None,
        };
        assert_eq!(
            read_one(&store, "anthropic", &entry, &clock, &fetcher)
                .await
                .unwrap()
                .0,
            Some(value)
        );
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
    }

    // Catches: a 429 cooldown is process-local or ignores a longer Retry-After.
    #[tokio::test]
    async fn auth_usage_retry_after_is_shared_and_preserves_last_value() {
        let home = tempfile::tempdir().unwrap();
        let a = QuotaStore::new(home.path());
        let b = QuotaStore::new(home.path());
        let clock = TestClock(AtomicU64::new(1_000_000));
        let entry = entry(2_000_000);
        let fetcher = FixtureFetcher {
            calls: AtomicUsize::new(0),
            status: 429,
            retry: Some("600".into()),
        };
        let headers = ego_ai::oauth::provider_auth("anthropic", "sk-ant-oat-test-only")
            .unwrap()
            .headers();
        let stale = parse_usage(
            "anthropic",
            &credential_fingerprint(&headers),
            &serde_json::to_vec(&fixture("anthropic")["body"]).unwrap(),
            0,
        )
        .unwrap();
        a.observe(stale.clone()).unwrap();
        let (quota, error) = read_one(&a, "anthropic", &entry, &clock, &fetcher)
            .await
            .unwrap();
        assert_eq!(quota, Some(stale));
        assert_eq!(error.unwrap().retry_at, Some(1600000));
        clock.0.store(1599999, Ordering::SeqCst);
        let (_, error) = read_one(&b, "anthropic", &entry, &clock, &fetcher)
            .await
            .unwrap();
        assert_eq!(error.unwrap().retry_at, Some(1600000));
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            retry_at(Some("Thu, 12 Nov 2026 04:00:00 GMT"), 0),
            1794456000000
        );
    }

    // Catches: primary is assumed to be five-hour or endpoint identities leak to the contract.
    #[test]
    fn recorded_usage_bodies_keep_duration_and_exclude_identity() {
        for provider in ["anthropic", "openai-codex"] {
            let body = serde_json::to_vec(&fixture(provider)["body"]).unwrap();
            let limits = parse_usage(provider, "opaque", &body, 1000).unwrap();
            assert_eq!(limits.windows[0].name, "seven_day");
            assert_eq!(limits.windows[0].window_seconds, Some(604800));
            let json = serde_json::to_string(&limits).unwrap();
            assert!(!json.contains("email"));
            assert!(!json.contains("account_id"));
            assert!(!json.contains("REDACTED"));
            assert_eq!(limits.source, QuotaSource::Active);
        }
        for (seconds, expected) in [
            (10800, "five_hour"),
            (28800, "five_hour"),
            (432000, "seven_day"),
            (777600, "seven_day"),
            (2332800, "monthly"),
            (2764800, "monthly"),
            (60, "extra"),
        ] {
            assert_eq!(window_name(seconds, "extra"), expected);
        }
    }

    // Catches: two named accounts on one route share a snapshot or expose route/display labels.
    #[tokio::test]
    async fn named_connections_successful_usage_keeps_each_accounts_snapshot_and_id() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "[connections.account_a]\nname='Same label'\nvendor='anthropic'\naccess='oauth'\n\
             [connections.account_b]\nname='Same label'\nvendor='anthropic'\naccess='oauth'\n\
             [connections.anthropic]\nname='Same label'\nvendor='anthropic'\naccess='oauth'\n",
        )
        .unwrap();
        let auth = AuthStore::new(home.path());
        let store = QuotaStore::new(home.path());
        let fetcher = FixtureFetcher {
            calls: AtomicUsize::new(0),
            status: 200,
            retry: None,
        };
        // Distinct observation times identify each recorded successful response.
        // Future test-clock times keep execute's real-clock cache reads fresh without sleeps.
        for (id, access, observed_at) in [
            ("account_a", "sk-ant-oatTestOnly0101", 4_000_000_000_000),
            ("account_b", "sk-ant-oatTestOnly0102", 4_000_000_001_000),
            ("anthropic", "sk-ant-oatTestOnly0103", 4_000_000_002_000),
            ("legacy", "sk-ant-oat-test-legacy", 4_000_000_003_000),
        ] {
            let entry = AuthEntry::OAuth {
                access: access.to_owned().into(),
                refresh: None,
                expires: None,
                scope_binding: None,
            };
            let (quota, error) = read_one(
                &store,
                "anthropic",
                &entry,
                &TestClock(AtomicU64::new(observed_at)),
                &fetcher,
            )
            .await
            .unwrap();
            assert!(error.is_none());
            assert_eq!(quota.unwrap().observed_at, observed_at);
            let key = if id == "legacy" {
                "anthropic".to_owned()
            } else {
                crate::auth::connection_storage_key(id)
            };
            auth.put(&key, entry).unwrap();
        }
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 4);

        for (selected, expected) in [
            (
                None,
                vec![
                    (Some("account_a"), 4_000_000_000_000_u64),
                    (Some("account_b"), 4_000_000_001_000),
                    (Some("anthropic"), 4_000_000_002_000),
                ],
            ),
            (
                Some("connection:account_b"),
                vec![(Some("account_b"), 4_000_000_001_000)],
            ),
            (
                Some("connection:account_a"),
                vec![(Some("account_a"), 4_000_000_000_000)],
            ),
            (
                Some("connection:anthropic"),
                vec![(Some("anthropic"), 4_000_000_002_000)],
            ),
        ] {
            let output = execute(home.path(), selected, None, true, &CancelToken::new())
                .await
                .unwrap();
            if selected.is_none() {
                // Expected offline producer capture for the external machine consumer.
                print!("ACTIVE_QUOTA_CONTRACT_FIXTURE={output}");
                assert_eq!(
                    output,
                    include_str!("../tests/fixtures/1143-named-quota-output.json"),
                    "record the actual producer bytes used by settings consumers"
                );
            }
            let output: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(output["errors"], serde_json::json!([]), "{selected:?}");
            let quotas = output["rate_limits"].as_array().unwrap();
            assert_eq!(quotas.len(), expected.len(), "{selected:?}");
            for (id, observed_at) in expected {
                let quota = quotas
                    .iter()
                    .find(|quota| quota["connection"].as_str() == id)
                    .unwrap();
                assert_eq!(quota["provider"], "anthropic");
                assert_eq!(quota["observed_at"], observed_at, "{selected:?}: {id:?}");
                assert_eq!(quota["windows"][0]["name"], "seven_day");
                assert_eq!(quota["windows"][0]["used_percentage"], 82.0);
            }
        }
    }

    // Catches: named errors replace vendor with account ID, or invent a vendor for missing config.
    #[tokio::test]
    async fn named_connections_usage_errors_keep_vendor_separate_from_account() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), "[connections.expired]\nname='Subscription'\nvendor='openai'\naccess='oauth'\n[connections.missing]\nname='Missing'\nvendor='anthropic'\naccess='oauth'\n[connections.key]\nname='Key'\nvendor='anthropic'\naccess='key'\nkey_env='UNUSED_API_KEY'\n").unwrap();
        AuthStore::new(home.path())
            .put(&crate::auth::connection_storage_key("expired"), entry(0))
            .unwrap();
        for (id, vendor, state) in [
            ("expired", Some("openai"), "auth_invalid"),
            ("missing", Some("anthropic"), "auth_invalid"),
            ("key", Some("anthropic"), "unavailable"),
            ("orphan", None, "auth_invalid"),
        ] {
            let raw = execute(
                home.path(),
                Some(&crate::auth::connection_storage_key(id)),
                None,
                true,
                &CancelToken::new(),
            )
            .await
            .unwrap();
            assert!(!raw.contains("sk-ant-oat-test-only"));
            print!("ERROR_QUOTA_CONTRACT_FIXTURE={raw}");
            let output: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(output["rate_limits"], serde_json::json!([]));
            assert_eq!(output["errors"][0]["connection"], id);
            assert_eq!(output["errors"][0]["provider"].as_str(), vendor);
            assert_eq!(output["errors"][0]["state"], state);
        }
    }

    // Catches: a key connection is diagnosed as expired OAuth and told to run a login it refuses.
    #[tokio::test]
    async fn named_connections_key_usage_reports_unavailable_without_a_login_repair() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), "[connections.key_account]\nname='Key'\nvendor='anthropic'\naccess='key'\nkey_env='UNUSED_API_KEY'\n").unwrap();
        let output = execute(
            home.path(),
            Some("connection:key_account"),
            None,
            true,
            &CancelToken::new(),
        )
        .await
        .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["errors"][0]["provider"], "anthropic");
        assert_eq!(output["errors"][0]["connection"], "key_account");
        assert_eq!(output["errors"][0]["state"], "unavailable");
        assert!(
            output["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("API-key environment source")
        );
        assert!(
            !output["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("login")
        );
        assert!(output["rate_limits"].as_array().unwrap().is_empty());
    }

    // Catches: management parsing routes usage into login or loses --json/provider selection.
    #[test]
    fn auth_usage_is_an_explicit_non_generating_command() {
        let parsed = crate::cli::parse(
            ["ego", "auth", "usage", "anthropic", "--json"]
                .map(str::to_owned)
                .to_vec(),
        )
        .unwrap();
        let crate::cli::Invocation::Auth(args) = parsed else {
            panic!("not auth");
        };
        assert_eq!(args.action, crate::cli::AuthAction::Usage { json: true });
        assert_eq!(args.provider.as_deref(), Some("anthropic"));
    }
}
