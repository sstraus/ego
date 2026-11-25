//! Mint a Vertex AI access token from Google Application Default Credentials.

use super::*;
use ego_ai::google_adc;
use ego_ai::provider::CredentialStrategy;

/// The variable that names an ADC file, read first by every Google client.
pub(super) const CREDENTIALS_VAR: &str = "GOOGLE_APPLICATION_CREDENTIALS";
const GCLOUD_LOGIN: &str = "gcloud auth application-default login";
const PRINCIPAL_PREFIX: &str = "google-adc:v1:";

pub(super) fn has_principal_binding(binding: Option<&str>) -> bool {
    binding.is_some_and(|binding| binding.starts_with(PRINCIPAL_PREFIX))
}

pub(super) fn missing_principal_binding(connection: &str) -> AuthError {
    refusal(format!(
        "cannot refresh Vertex Gemini connection {connection}: it has no saved ADC principal binding; \
         retrying unchanged cannot help. Select the intended ADC file and run `ego auth --connection {connection} login`"
    ))
}

pub(super) fn changed_principal(connection: &str) -> AuthError {
    refusal(format!(
        "cannot refresh Vertex Gemini connection {connection}: the ADC file authenticated a different Google principal; \
         retrying unchanged cannot help. Restore the original ADC file with {CREDENTIALS_VAR} and retry, \
         or create a separate Vertex Gemini connection for the new principal"
    ))
}

/// Whether the row for `id` authenticates with a Google Cloud token.
///
/// Read from the provider table, like [`environment_var`], so the login and
/// refresh paths cannot name a different provider from the row.
pub(super) fn is_google_cloud(id: &str) -> bool {
    crate::model::rows(&Default::default())
        .ok()
        .and_then(|rows| rows.into_iter().find(|row| row.id() == id))
        .is_some_and(|row| {
            matches!(&row.credential, CredentialStrategy::GoogleCloud { .. })
                || matches!(&row.credential, CredentialStrategy::OAuth { provider }
                    if provider == ego_ai::provider::VERTEX_GEMINI)
        })
}

/// Where the ADC file is: the variable, else the file gcloud writes.
fn credentials_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(CREDENTIALS_VAR).filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let config = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?).join("gcloud")
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".config/gcloud")
    };
    Some(config.join("application_default_credentials.json"))
}

fn refusal(reason: impl Into<String>) -> AuthError {
    AuthError::MissingCredential {
        program: "Google Application Default Credentials".to_string(),
        reason: reason.into(),
    }
}

/// One fresh access token, as the store entry login and refresh both write.
///
/// The entry holds no refresh secret: each refresh reads the ADC file again,
/// so revoking or replacing that file takes effect at the next refresh.
pub(super) async fn token_entry(
    http: &dyn ego_ai::http::Http,
    bind_principal: bool,
) -> Result<AuthEntry, AuthError> {
    let path = credentials_path().ok_or_else(|| {
        refusal(format!(
            "no ADC file location is known; set {CREDENTIALS_VAR} or run `{GCLOUD_LOGIN}`"
        ))
    })?;
    let bytes = fs::read(&path).map_err(|error| {
        refusal(format!(
            "cannot read {}: {error}; run `{GCLOUD_LOGIN}` or set {CREDENTIALS_VAR}",
            path.display()
        ))
    })?;
    let adc = google_adc::parse(&bytes).map_err(|error| refusal(error.to_string()))?;
    let request = adc
        .token_request()
        .map_err(|error| refusal(error.to_string()))?;
    let token = google_adc::exchange(http, request, now_ms())
        .await
        .map_err(|error| refusal(error.to_string()))?;
    let values = token.into_values();
    let scope_binding = if bind_principal {
        Some(vertex_principal_binding(&adc, http, values.access.as_bytes()).await?)
    } else {
        None
    };
    Ok(AuthEntry::OAuth {
        access: values.access.into(),
        refresh: None,
        expires: values.expires,
        scope_binding,
    })
}

async fn vertex_principal_binding(
    adc: &google_adc::Adc,
    http: &dyn ego_ai::http::Http,
    access: &[u8],
) -> Result<String, AuthError> {
    use google_adc::{AdcError, UserInfoFailure};
    let binding = adc.principal_binding(http, access).await.map_err(|error| {
        let guidance = match error {
            AdcError::Identity {
                cause: UserInfoFailure::Transient,
                ..
            } => "Retrying can help. Retry the Vertex Gemini operation when Google UserInfo is reachable.",
            AdcError::Identity {
                cause: UserInfoFailure::Access,
                ..
            } => concat!(
                "Retrying unchanged cannot help. Re-run `gcloud auth application-default login` with default scopes, ",
                "or include `openid` and `https://www.googleapis.com/auth/cloud-platform` in your custom scopes."
            ),
            _ => "Retrying unchanged cannot help. Check the network policy and Google's UserInfo response before retrying.",
        };
        refusal(format!(
            "could not verify the Vertex Gemini ADC principal: {error}. {guidance}"
        ))
    })?;
    Ok(format!("{PRINCIPAL_PREFIX}{binding}"))
}

/// Refuse an unusable cloud address before publishing a successful login.
/// This is the same declared project/location contract the request adapter uses.
pub(super) fn validate_context(route: &str) -> Result<(), AuthError> {
    let row = crate::model::rows(&Default::default())
        .map_err(|error| AuthError::InvalidConfig(error.to_string()))?
        .into_iter()
        .find(|row| row.id() == route);
    let Some(CredentialStrategy::GoogleCloud {
        project_var,
        location_var,
        ..
    }) = row.map(|row| row.credential)
    else {
        return Ok(());
    };
    let project = EnvCredentials
        .secret(&project_var)
        .map_err(|_| AuthError::InvalidConfig(format!("set {project_var} before cloud login")))?;
    let location = EnvCredentials.secret(&location_var).ok();
    let project =
        secret_text(&project).map_err(|reason| AuthError::InvalidConfig(reason.into()))?;
    let location = location
        .as_ref()
        .map(secret_text)
        .transpose()
        .map_err(|reason| AuthError::InvalidConfig(reason.into()))?
        .unwrap_or(ego_ai::provider::VERTEX_DEFAULT_LOCATION);
    ego_ai::provider::vertex_base(project, location).map_err(|_| {
        AuthError::InvalidConfig(format!(
            "invalid cloud address: check {project_var} and {location_var}"
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests;
