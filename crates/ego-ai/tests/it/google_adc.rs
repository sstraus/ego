//! Google Application Default Credentials to a Vertex AI access token.

use ego_ai::google_adc::{self, AdcError, TOKEN_URL};
use ego_ai::oauth::OAuthToken;

fn form(body: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

fn value<'a>(fields: &'a [(String, String)], name: &str) -> &'a str {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .unwrap_or_else(|| panic!("no `{name}` in {fields:?}"))
}

/// Catches: a refresh grant that omits the file's own client, which Google
/// refuses with `invalid_client`, or a request sent to another endpoint.
#[test]
fn an_authorized_user_file_refreshes_with_its_own_client() {
    let adc = google_adc::parse(
        br#"{"type":"authorized_user","client_id":"cid","client_secret":"csecret",
             "refresh_token":"rtoken","quota_project_id":"q"}"#,
    )
    .expect("a gcloud ADC file");
    assert!(!format!("{adc:?}").contains("rtoken"), "{adc:?}");
    let request = adc.token_request().expect("a request");
    assert_eq!(request.url.as_str(), TOKEN_URL);
    let fields = form(request.body.as_deref().expect("a form body"));
    assert_eq!(value(&fields, "grant_type"), "refresh_token");
    assert_eq!(value(&fields, "client_id"), "cid");
    assert_eq!(value(&fields, "client_secret"), "csecret");
    assert_eq!(value(&fields, "refresh_token"), "rtoken");
}

/// Catches: a service-account key or another ADC type sent to Google as if it
/// were a user credential, instead of a refusal that names its type.
#[test]
fn other_credential_types_are_named_and_not_sent() {
    for kind in ["service_account", "external_account"] {
        let file = format!(r#"{{"type":"{kind}","client_email":"a","private_key":"k"}}"#);
        assert_eq!(
            google_adc::parse(file.as_bytes()).map(drop),
            Err(AdcError::Unsupported(kind.to_string()))
        );
    }
}

/// Catches: an expiry read as milliseconds, which makes the store refresh on
/// every request or keep a dead token for a thousand hours, and a refusal
/// that drops Google's own reason.
#[test]
fn the_answer_becomes_a_token_with_an_absolute_expiry_or_googles_reason() {
    let token = google_adc::read_token(
        200,
        br#"{"access_token":"ya29.x","expires_in":3599,"token_type":"Bearer"}"#,
        10_000,
    )
    .expect("a token");
    match token {
        OAuthToken::OAuth {
            access,
            refresh,
            expires,
        } => {
            assert_eq!(access, "ya29.x");
            assert_eq!(refresh, None);
            assert_eq!(expires, Some(10_000 + 3_599_000));
        }
        OAuthToken::ApiKey { .. } => panic!("an access token is not an API key"),
    }
    let refused = google_adc::read_token(
        400,
        br#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        0,
    );
    assert_eq!(
        refused.map(drop),
        Err(AdcError::Refused {
            status: 400,
            message: "invalid_grant: Token has been expired or revoked.".to_string(),
        })
    );
}
