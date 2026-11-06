//! Secret-free diagnostics for provider HTTP request and credential values.

use ego_ai::http::{HttpRequest, Method};
use ego_ai::provider::{CredentialStrategy, Credentials, MissingCredential, Secret};
use ego_core::permit::HttpHeader;
use url::Url;

/// The checklist item is "no credentials or request bodies reachable from
/// `PublicEvent`", and `Secret`'s redaction does not reach this far.
#[test]
fn a_formatted_request_prints_neither_the_credential_nor_the_body() {
    let printed = format!(
        "{:?}",
        HttpRequest {
            url: Url::parse("https://api.example.com/v1/chat/completions").unwrap(),
            method: Method::Post,
            headers: vec![
                HttpHeader::new("authorization", b"Bearer sk-live-SECRET".to_vec()).unwrap(),
                HttpHeader::new("content-type", b"application/json".to_vec()).unwrap(),
            ],
            body: Some(br#"{"messages":[{"role":"user","content":"PROMPT"}]}"#.to_vec()),
        }
    );

    assert!(
        !printed.contains("sk-live-SECRET") && !printed.contains("Bearer"),
        "a credential reached a formatter: {printed}"
    );
    assert!(
        !printed.contains("PROMPT") && !printed.contains("messages"),
        "a request body reached a formatter: {printed}"
    );
    assert!(printed.contains("api.example.com"), "{printed}");
    assert!(printed.contains("authorization"), "{printed}");
    assert!(printed.contains("content-type"), "{printed}");
    assert!(printed.contains("<49 bytes>"), "{printed}");
}

#[test]
fn a_resolved_credential_prints_no_part_of_the_credential() {
    let credential = CredentialStrategy::BearerFromEnv {
        var: "PROVIDER_KEY".to_string(),
    }
    .resolve(&Configured)
    .expect("the fake holds every variable");
    let printed = format!("{credential:?}");

    assert!(
        !printed.contains(LIVE_KEY),
        "a credential reached a formatter: {printed}"
    );
    assert!(
        !printed.contains("Bearer"),
        "the credential's header value reached a formatter: {printed}"
    );
    assert!(printed.contains("authorization"), "{printed}");
}

const LIVE_KEY: &str = "sk-live-a-string-nothing-else-here-contains";

struct Configured;

impl Credentials for Configured {
    fn secret(&self, _var: &str) -> Result<Secret, MissingCredential> {
        Ok(LIVE_KEY.into())
    }
}
