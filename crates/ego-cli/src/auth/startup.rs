//! Read-only authentication description: no login, renewal, or command execution.

use ego_ai::provider::CredentialStrategy;
use ego_core::config::Effective;

use super::{AuthEntry, AuthStore};
use crate::onboarding::{EnvironmentPresence, ProcessEnvironment};

pub(crate) fn summary(provider: &str, effective: &Effective) -> String {
    summary_at(
        provider,
        effective,
        super::optional_ego_home()
            .as_deref()
            .map(AuthStore::new)
            .as_ref(),
        &ProcessEnvironment,
    )
}

fn summary_at(
    provider: &str,
    effective: &Effective,
    store: Option<&AuthStore>,
    environment: &dyn EnvironmentPresence,
) -> String {
    let strategy = crate::model::rows(&effective.auth)
        .ok()
        .and_then(|rows| rows.into_iter().find(|row| row.id() == provider))
        .map(|row| row.credential);
    let Some(strategy) = strategy else {
        return "unavailable · /login".into();
    };
    let selected = ego_core::config::select_connection(
        &effective.connections,
        effective.connection.as_deref(),
        Some(super::connection_vendor(provider)),
    );
    match selected {
        Err(_) => return "unavailable (account selection) · select --account N".into(),
        Ok(Some((id, connection))) => {
            if super::connection_route(connection) != provider {
                return format!(
                    "unavailable · selected account requires route {}",
                    super::connection_route(connection)
                );
            }
            return describe_connection(id, connection, provider, &strategy, store, environment);
        }
        Ok(None) => {}
    }
    describe(
        provider,
        &strategy,
        effective
            .auth
            .get(provider)
            .and_then(|auth| auth.credential_command.as_deref()),
        environment,
    )
}

fn describe_connection(
    id: &str,
    connection: &ego_core::config::ConnectionConfig,
    provider: &str,
    strategy: &CredentialStrategy,
    store: Option<&AuthStore>,
    environment: &dyn EnvironmentPresence,
) -> String {
    use ego_core::config::ConnectionAccess;
    if connection.access == ConnectionAccess::Key {
        if strategy.environment_var().is_none() {
            return "unavailable · selected account requires a key route".to_owned();
        }
        let Some(source) = connection.key_env.as_deref() else {
            return if super::key_connection_present(id, connection, store, environment) {
                "API key · stored credential".to_owned()
            } else {
                format!("missing API key · {}", super::renewal_hint(provider))
            };
        };
        return if environment.has_nonblank(source) {
            format!("API key · environment ({source})")
        } else {
            format!("missing API key · set {source} for the selected account")
        };
    }
    if !super::oauth_connection_supported(strategy) {
        return "unavailable · selected account requires an OAuth route".to_owned();
    }
    let remedy = super::renewal_hint(provider);
    let entries = match store.map(AuthStore::read_observed).transpose() {
        Ok(entries) => entries,
        Err(_) => return format!("unavailable (credential store) · {remedy}"),
    };
    let entry = entries
        .as_ref()
        .and_then(|entries| entries.get(&super::connection_storage_key(id)));
    let Some(AuthEntry::OAuth { .. }) = entry else {
        return format!("not signed in · {remedy}");
    };
    if let CredentialStrategy::GoogleCloud { project_var, .. } = strategy
        && !environment.has_nonblank(project_var)
    {
        return format!("missing cloud project · set {project_var} for the selected account");
    }
    let method = if provider == "openai-codex" {
        "subscription (OAuth)"
    } else {
        "OAuth"
    };
    format!("{method} · stored credential")
}

fn describe(
    provider: &str,
    strategy: &CredentialStrategy,
    command: Option<&str>,
    environment: &dyn EnvironmentPresence,
) -> String {
    use CredentialStrategy::*;
    // An unbound run can use this explicit environment token, never a saved slot.
    if provider == "openai-codex"
        && environment.has_nonblank(ego_ai::provider::OPENAI_CODEX_ACCESS_TOKEN_ENV)
    {
        return "access token · env token".into();
    }
    match strategy {
        None => return "none (local provider)".into(),
        BearerFromEnv { var } | HeaderFromEnv { var, .. } => {
            return if environment.has_nonblank(var) {
                "API key · environment".into()
            } else {
                "missing API key · /login".into()
            };
        }
        _ => {}
    }
    match strategy {
        OAuthOrEnv { var, .. } if environment.has_nonblank(var) => "API key · environment".into(),
        Anthropic {
            oauth_var,
            api_key_var,
            ..
        } => {
            if environment.has_nonblank(oauth_var) {
                "access token · environment".into()
            } else if environment.has_nonblank(api_key_var) {
                "API key · environment".into()
            } else {
                missing(command)
            }
        }
        _ => missing(command),
    }
}

fn missing(command: Option<&str>) -> String {
    if command.is_some() {
        "not signed in · command login configured · /login".into()
    } else {
        "not signed in · /login".into()
    }
}

#[cfg(test)]
mod tests;
