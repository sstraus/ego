//! Call-time credentials and OS canonicalization for the shared Jev client.
use super::{DecisionError, DecisionsClient};
use crate::http::{Http, bearer_header};
use ego_core::{
    config::{JEV_ENDPOINT, JevAuth, JevConfig, JevSettings, JevUse},
    credentials::SecretMaterial,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

/// Configuration retains credential references only, never resolved keys.
pub struct ConfiguredDecisions {
    config: JevConfig,
    home: PathBuf,
    timeout: Duration,
}
impl ConfiguredDecisions {
    /// Construction does not read credentials or make a request.
    pub fn new(config: JevConfig, home: PathBuf, timeout: Duration) -> Self {
        Self {
            config,
            home,
            timeout,
        }
    }
    /// Canonicalize both the session and explicit root overrides before selection.
    pub fn settings(&self, root: &Path) -> Result<JevSettings, DecisionError> {
        let root = std::fs::canonicalize(root).map_err(|_| DecisionError::InvalidConfiguration)?;
        let entries = self
            .config
            .roots
            .iter()
            .map(|entry| {
                let raw = entry.path.strip_suffix("/**").unwrap_or(&entry.path);
                let path = if raw == "~" {
                    self.home.clone()
                } else if let Some(tail) = raw.strip_prefix("~/") {
                    self.home.join(tail)
                } else {
                    PathBuf::from(raw)
                };
                if !path.is_absolute() {
                    return Err(DecisionError::InvalidConfiguration);
                }
                std::fs::canonicalize(path).map_err(|_| DecisionError::InvalidConfiguration)
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.config
            .select(&root, &entries)
            .map_err(|_| DecisionError::InvalidConfiguration)
    }
    /// Off or uncalibrated uses return None without resolving auth or invoking HTTP.
    #[allow(clippy::too_many_arguments)]
    pub async fn ask(
        &self,
        http: &dyn Http,
        root: &Path,
        usage: JevUse,
        calibrated: bool,
        state: &Value,
        questions: &BTreeMap<String, String>,
    ) -> Result<Option<BTreeMap<String, f64>>, DecisionError> {
        if !calibrated {
            return Ok(None);
        }
        let settings = self.settings(root)?;
        if !settings.policy(usage).on {
            return Ok(None);
        }
        let headers = match &settings.auth {
            JevAuth::ApiKey { env } => {
                let key = if settings.endpoint == JEV_ENDPOINT && env == "OPENROUTER_API_KEY" {
                    let file = self.home.join("workspace/example-project/.env");
                    let mut found = None;
                    for entry in dotenvy::from_path_iter(file)
                        .map_err(|_| DecisionError::CredentialsUnavailable)?
                    {
                        let (name, value) =
                            entry.map_err(|_| DecisionError::CredentialsUnavailable)?;
                        if name == *env {
                            found = Some(SecretMaterial::from(value));
                        }
                    }
                    found.ok_or(DecisionError::CredentialsUnavailable)?
                } else {
                    SecretMaterial::from(
                        std::env::var(env).map_err(|_| DecisionError::CredentialsUnavailable)?,
                    )
                };
                if key.expose_secret_bytes().is_empty() {
                    return Err(DecisionError::CredentialsUnavailable);
                }
                vec![bearer_header(key.expose_secret_bytes())]
            }
            JevAuth::Header { name, value } => vec![
                SecretMaterial::from(value.as_str())
                    .into_secret_header(name)
                    .map_err(|_| DecisionError::InvalidConfiguration)?,
            ],
        };
        let client = DecisionsClient::new(
            settings
                .endpoint
                .parse()
                .map_err(|_| DecisionError::InvalidConfiguration)?,
            settings.model,
            self.timeout,
        )?;
        client
            .ask_with_headers(http, headers, state, questions)
            .await
            .map(Some)
    }
}
