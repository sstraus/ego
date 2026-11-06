//! Mint a Vertex AI access token from Google Application Default Credentials.

use super::*;
use ego_ai::google_adc;
use ego_ai::provider::CredentialStrategy;

/// The variable that names an ADC file, read first by every Google client.
pub(super) const CREDENTIALS_VAR: &str = "GOOGLE_APPLICATION_CREDENTIALS";
const GCLOUD_LOGIN: &str = "gcloud auth application-default login";

/// Whether the row for `id` authenticates with a Google Cloud token.
///
/// Read from the provider table, like [`environment_var`], so the login and
/// refresh paths cannot name a different provider from the row.
pub(super) fn is_google_cloud(id: &str) -> bool {
    crate::model::rows(&Default::default())
        .ok()
        .and_then(|rows| rows.into_iter().find(|row| row.id() == id))
        .is_some_and(|row| matches!(row.credential, CredentialStrategy::GoogleCloud { .. }))
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
pub(super) async fn token_entry(http: &dyn ego_ai::http::Http) -> Result<AuthEntry, AuthError> {
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
    Ok(AuthEntry::OAuth {
        access: values.access.into(),
        refresh: None,
        expires: values.expires,
    })
}
