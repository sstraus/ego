//! Contracts for A2A endpoint policy and shared bearer authentication.

use ego_a2a::auth::{Credential, CredentialRefusal, TOKEN_VAR};
use ego_core::credentials::SecretMaterial;
use ego_core::endpoint::BearerCredential;

#[test]
fn empty_a2a_secret_is_refused_and_debug_never_exposes_secret_bytes() {
    assert_eq!(
        Credential::from_env(|_| None).unwrap_err(),
        CredentialRefusal::Unset
    );
    assert_eq!(
        Credential::from_env(|_| Some(String::new())).unwrap_err(),
        CredentialRefusal::Empty
    );
    let credential = Credential::from_env(|name| {
        assert_eq!(name, TOKEN_VAR);
        Some("a2a-secret-never-debugged".to_owned())
    })
    .unwrap();
    let rendered = format!("{credential:?}");
    assert!(!rendered.contains("a2a-secret-never-debugged"));
}

#[test]
fn shared_bearer_matcher_owns_secret_bytes_without_parsing_http() {
    let credential = BearerCredential::new(SecretMaterial::new(b"shared-secret".to_vec()));
    assert!(credential.matches(b"shared-secret"));
    assert!(!credential.matches(b"Bearer shared-secret"));
    assert!(!credential.matches(b""));
    assert!(!format!("{credential:?}").contains("shared-secret"));
}
