//! First-run model selection from presence facts that reveal no credentials.
//!
//! This policy runs only after ordinary configuration resolution found no
//! model. Callers retain the explicit/persisted-model bypass and assemble the
//! catalogue snapshot without executing credential commands.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ego_ai::catalogue::{
    Catalogue, ModelCatalogueSnapshot, ModelDiscoveryReport, ModelSlug, OLLAMA,
};
use ego_ai::http::Http;
use ego_ai::provider::{CredentialStrategy, ProviderRow};
use ego_ai::resolver::Enumeration;
use ego_core::config::{
    ManagedConfigKey, ManagedConfigValue, ModelRef, UserConfigError, UserConfigMutation,
    UserConfigPublication, UserConfigStore, UserConfigTarget,
};
use ego_core::generator::types::ThinkingLevel;
use url::Host;

use crate::cli::UsageError;
use crate::model_picker::{ModelSwitchRequest, available_entry};

/// The process observation startup policy is allowed to make.
///
/// Neither method returns a value. This keeps credential material outside the
/// first-run decision and gives tests a deterministic boundary without
/// mutating the process environment.
pub(crate) trait EnvironmentPresence: Send + Sync {
    fn has_nonblank(&self, name: &str) -> bool;
    fn is_set(&self, name: &str) -> bool;
}

/// Presence observations for the current process.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ProcessEnvironment;

impl EnvironmentPresence for ProcessEnvironment {
    fn has_nonblank(&self, name: &str) -> bool {
        std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
    }

    fn is_set(&self, name: &str) -> bool {
        std::env::var_os(name).is_some()
    }
}

/// Whether one provider can attempt its normal authentication path.
///
/// A saved OAuth identity counts as present even if its access token is stale:
/// refresh and authentication remain the real run's responsibility. This
/// function never reads a secret, executes a command, refreshes, or logs in.
pub(crate) fn credential_present(
    strategy: &CredentialStrategy,
    environment: &dyn EnvironmentPresence,
    observed_provider_ids: &BTreeSet<String>,
) -> bool {
    match strategy {
        CredentialStrategy::None => true,
        CredentialStrategy::BearerFromEnv { var }
        | CredentialStrategy::HeaderFromEnv { var, .. } => environment.has_nonblank(var),
        CredentialStrategy::OAuth { provider } => observed_provider_ids.contains(provider),
        CredentialStrategy::GoogleCloud {
            provider,
            project_var,
            ..
        } => observed_provider_ids.contains(provider) && environment.has_nonblank(project_var),
        CredentialStrategy::OAuthOrEnv { provider, var } => {
            observed_provider_ids.contains(provider) || environment.has_nonblank(var)
        }
        CredentialStrategy::Anthropic {
            provider,
            api_key_var,
            oauth_var,
        } => {
            observed_provider_ids.contains(provider)
                || environment.has_nonblank(api_key_var)
                || environment.has_nonblank(oauth_var)
        }
    }
}

/// Enumerate an Ollama row through its resolver and an already-guarded call.
///
/// The HTTP adapter owns the deadline and admitted route. This seam adds the
/// startup-specific authority rule: ambient discovery stays on loopback, while
/// an explicitly supplied `OLLAMA_HOST` may name another guarded endpoint.
pub(crate) async fn discover_ollama(
    row: &ProviderRow,
    http: &Arc<dyn Http>,
    ollama_host_explicit: bool,
) -> Result<ModelDiscoveryReport, String> {
    if row.id() != OLLAMA {
        return Err(format!(
            "startup Ollama discovery received provider `{}`",
            row.id()
        ));
    }
    if !ollama_host_explicit && !is_loopback(row) {
        return Err(format!(
            "ambient Ollama discovery refuses non-loopback endpoint `{}`; set OLLAMA_HOST explicitly to authorize it",
            row.base()
        ));
    }

    let catalogue = Catalogue::new(std::iter::once(row.clone()));
    let row = catalogue.row(OLLAMA).map_err(|error| error.to_string())?;
    let enumeration = Enumeration {
        catalogue: &catalogue,
        row,
        http,
        credential: None,
    };
    row.resolver.enumerate(&enumeration).await
}

fn is_loopback(row: &ProviderRow) -> bool {
    match row.base().host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// The only outcomes for startup after model resolution found no model.
#[derive(Debug)]
pub(crate) enum StartupDecision {
    /// Interactive: use the sole usable provider's default and persist it.
    AutoSelect(ModelRef),
    /// Headless: use the sole usable provider's default for this run only.
    /// A script must not mutate the operator's configuration.
    UseForRun(ModelRef),
    Pick(Arc<ModelCatalogueSnapshot>, Option<UsageError>),
    Headless(UsageError),
}

/// Choose policy from observed usability without selecting by catalogue order.
pub(crate) fn decide_missing_model(
    snapshot: Arc<ModelCatalogueSnapshot>,
    defaults: &BTreeMap<String, ModelSlug>,
    usable_provider_ids: &BTreeSet<String>,
    _thinking: Option<ThinkingLevel>,
    interactive: bool,
) -> StartupDecision {
    let sole = sole_provider_default(&snapshot, defaults, usable_provider_ids);
    match (interactive, sole) {
        (true, Ok(model)) => StartupDecision::AutoSelect(model),
        (false, Ok(model)) => StartupDecision::UseForRun(model),
        (true, Err(notice)) => StartupDecision::Pick(snapshot, notice),
        (false, Err(_)) => {
            StartupDecision::Headless(headless_refusal(&snapshot, defaults, usable_provider_ids))
        }
    }
}

/// The selectable default of the only usable provider, or the notice that
/// explains why the picker must ask instead (`None` with zero or several
/// usable providers: the picker rows already name each provider's remedy).
fn sole_provider_default(
    snapshot: &ModelCatalogueSnapshot,
    defaults: &BTreeMap<String, ModelSlug>,
    usable_provider_ids: &BTreeSet<String>,
) -> Result<ModelRef, Option<UsageError>> {
    if usable_provider_ids.len() != 1 {
        return Err(None);
    }

    let provider = usable_provider_ids
        .iter()
        .next()
        .expect("a singleton provider set has one member");
    let Some(default) = defaults.get(provider) else {
        return Err(Some(missing_default(provider)));
    };
    if default.provider != *provider {
        return Err(Some(UsageError(format!(
            "default model '{default}' does not belong to the available provider '{provider}'; choose an available model below"
        ))));
    }
    if let Err(error) = available_entry(snapshot, &default.to_string()) {
        return Err(Some(UsageError(format!(
            "default model '{default}' for provider '{provider}' cannot be selected: {error}; choose an available model below"
        ))));
    }
    Ok(ModelRef(default.to_string()))
}

/// One provider's credential readiness, observed from presence only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderReadiness {
    pub(crate) provider: String,
    pub(crate) ready: bool,
    /// The remedy when not ready, or a caveat a ready row still has. Never a
    /// credential value.
    pub(crate) note: Option<String>,
}

impl std::fmt::Display for ProviderReadiness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = if self.ready { "ready" } else { "not ready" };
        match &self.note {
            Some(note) => write!(f, "{}: {state} — {note}", self.provider),
            None => write!(f, "{}: {state}", self.provider),
        }
    }
}

/// Readiness of every provider row, with the same presence rule as first-run
/// selection. It reads no secret, runs no command, and calls no provider.
pub(crate) fn provider_readiness(
    rows: &[ProviderRow],
    environment: &dyn EnvironmentPresence,
    observed_provider_ids: &BTreeSet<String>,
) -> Vec<ProviderReadiness> {
    rows.iter()
        .map(|row| {
            let id = row.id();
            let ready = credential_present(&row.credential, environment, observed_provider_ids);
            let login = format!("run 'ego auth login {id}'");
            let note = match &row.credential {
                CredentialStrategy::None => {
                    Some("no credential; needs a running local server".to_owned())
                }
                _ if ready => None,
                CredentialStrategy::BearerFromEnv { var }
                | CredentialStrategy::HeaderFromEnv { var, .. } => Some(format!("set {var}")),
                CredentialStrategy::OAuth { .. } => Some(login),
                CredentialStrategy::GoogleCloud { project_var, .. } => {
                    Some(format!("set {project_var} and {login}"))
                }
                CredentialStrategy::OAuthOrEnv { var, .. } => Some(format!("set {var} or {login}")),
                CredentialStrategy::Anthropic { api_key_var, .. } => {
                    Some(format!("set {api_key_var} or {login}"))
                }
            };
            ProviderReadiness {
                provider: id.to_owned(),
                ready,
                note,
            }
        })
        .collect()
}

/// Current readiness for the setup guide, from this process's environment and
/// the saved subscription identities under `home`.
pub(crate) fn current_provider_readiness(
    home: &std::path::Path,
) -> Result<Vec<ProviderReadiness>, String> {
    let auth = crate::config::auth().map_err(|error| error.to_string())?;
    let rows = crate::model::rows(&auth).map_err(|error| error.to_string())?;
    let observed = crate::auth::AuthStore::new(home)
        .observed_provider_ids()
        .map_err(|error| error.to_string())?;
    Ok(provider_readiness(&rows, &ProcessEnvironment, &observed))
}

fn missing_default(provider: &str) -> UsageError {
    UsageError(format!(
        "provider '{provider}' has no startup default; choose an available model below"
    ))
}

fn headless_refusal(
    snapshot: &ModelCatalogueSnapshot,
    defaults: &BTreeMap<String, ModelSlug>,
    usable_provider_ids: &BTreeSet<String>,
) -> UsageError {
    let providers = if usable_provider_ids.is_empty() {
        "none".to_owned()
    } else {
        usable_provider_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let exact = usable_provider_ids
        .iter()
        .filter_map(|provider| defaults.get(provider))
        .filter(|slug| available_entry(snapshot, &slug.to_string()).is_ok())
        .map(|slug| format!("'--model {slug}'"))
        .collect::<Vec<_>>();
    let remedy = if exact.is_empty() {
        "'--model PROVIDER/MODEL'".to_owned()
    } else {
        exact.join("; ")
    };
    UsageError(format!(
        "no model is selected; detected usable providers: {providers}; choose one with {remedy}"
    ))
}

/// Atomically publish the startup choice before any session is materialized.
pub(crate) fn persist_startup_selection(
    store: &dyn UserConfigStore,
    request: &ModelSwitchRequest,
) -> Result<UserConfigPublication, UserConfigError> {
    if !request.is_persistent() {
        return Err(UserConfigError::new(
            "a startup model selection must be persistent",
        ));
    }
    let result = store.mutate(UserConfigMutation::ModelAndThinking {
        model: request.target().0,
        thinking: request.thinking(),
    })?;
    Ok(result.publication)
}

/// Atomically publish only the automatically selected model.
///
/// The model's reasoning default is derived capability authority, not an
/// operator choice, so automatic onboarding must not turn it into a durable
/// explicit override.
pub(crate) fn persist_startup_model(
    store: &dyn UserConfigStore,
    model: &ModelRef,
) -> Result<UserConfigPublication, UserConfigError> {
    let result = store.mutate(UserConfigMutation::ManagedSet {
        target: UserConfigTarget::Base,
        key: ManagedConfigKey::Model,
        value: ManagedConfigValue::Model(model.clone()),
    })?;
    Ok(result.publication)
}

#[cfg(test)]
mod tests;
