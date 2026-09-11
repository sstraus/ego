//! Contracts for ACP bearer authentication before protocol routing.

use ego_acp::auth::{Credential, CredentialRefusal, TOKEN_VAR};

#[test]
fn empty_acp_secret_is_refused_and_debug_is_redacted() {
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
        Some("acp-secret-never-debugged".to_owned())
    })
    .unwrap();
    assert!(!format!("{credential:?}").contains("acp-secret-never-debugged"));
}

#[test]
fn acp_credential_matches_shared_bearer_bytes_only() {
    let credential = Credential::from_env(|_| Some("shared-secret".to_owned())).unwrap();
    assert!(credential.admits(b"Bearer shared-secret"));
    assert!(!credential.admits(b"shared-secret"));
    assert!(!credential.admits(b"Bearer wrong"));
}
