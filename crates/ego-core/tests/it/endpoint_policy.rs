//! Contract for shared inbound endpoint policy.

use ego_core::credentials::SecretMaterial;
use ego_core::endpoint::BearerCredential;

#[test]
fn shared_bearer_matcher_compares_secret_bytes_without_http_parsing() {
    let expected = BearerCredential::new(SecretMaterial::new(b"shared-secret".to_vec()));
    assert!(expected.matches(b"shared-secret"));
    assert!(!expected.matches(b"shared-secreT"));
    assert!(!expected.matches(b"Bearer shared-secret"));
    assert!(!expected.matches(b"shared-secret "));
}

#[test]
fn empty_bearer_secret_admits_nobody_and_debug_redacts_secret_bytes() {
    let empty = BearerCredential::new(SecretMaterial::new(Vec::new()));
    assert!(!empty.matches(b""));
    assert!(!empty.matches(b"anything"));

    let rendered = format!("{empty:?}");
    assert!(!rendered.contains("shared-secret"));
    assert!(!rendered.contains("BearerCredential(["));
}
