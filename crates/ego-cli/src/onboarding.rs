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
    ModelRef, UserConfigError, UserConfigMutation, UserConfigPublication, UserConfigStore,
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

/// The only three outcomes for startup after model resolution found no model.
#[derive(Debug)]
pub(crate) enum StartupDecision {
    AutoSelect(ModelSwitchRequest),
    Pick(Arc<ModelCatalogueSnapshot>, Option<UsageError>),
    Headless(UsageError),
}

/// Choose policy from observed usability without selecting by catalogue order.
pub(crate) fn decide_missing_model(
    snapshot: Arc<ModelCatalogueSnapshot>,
    defaults: &BTreeMap<String, ModelSlug>,
    usable_provider_ids: &BTreeSet<String>,
    thinking: ThinkingLevel,
    interactive: bool,
) -> StartupDecision {
    if !interactive {
        return StartupDecision::Headless(headless_refusal(
            &snapshot,
            defaults,
            usable_provider_ids,
        ));
    }
    if usable_provider_ids.len() != 1 {
        return StartupDecision::Pick(snapshot, None);
    }

    let provider = usable_provider_ids
        .iter()
        .next()
        .expect("a singleton provider set has one member");
    let Some(default) = defaults.get(provider) else {
        return StartupDecision::Pick(snapshot, Some(missing_default(provider)));
    };
    if default.provider != *provider {
        return StartupDecision::Pick(
            snapshot,
            Some(UsageError(format!(
                "startup default `{default}` does not belong to sole usable provider `{provider}`; change `{provider}` under `[default_model]` in models.toml or choose an available row"
            ))),
        );
    }
    if let Err(error) = available_entry(&snapshot, &default.to_string()) {
        return StartupDecision::Pick(
            snapshot,
            Some(UsageError(format!(
                "startup default `{default}` for sole usable provider `{provider}` cannot be selected: {error}; change `{provider}` under `[default_model]` in models.toml or choose an available row"
            ))),
        );
    }

    StartupDecision::AutoSelect(ModelSwitchRequest::persistent(
        ModelRef(default.to_string()),
        thinking,
    ))
}

fn missing_default(provider: &str) -> UsageError {
    UsageError(format!(
        "sole usable provider `{provider}` has no startup default; add `{provider} = \"{provider}/model\"` under `[default_model]` in models.toml or choose an available row"
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
        .map(|slug| format!("`--model {slug}` or `ego config set model='\"{slug}\"'`"))
        .collect::<Vec<_>>();
    let remedy = if exact.is_empty() {
        "`--model <provider/model>` or `ego config set model='\"<provider/model>\"'`".to_owned()
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

#[cfg(test)]
mod tests;
