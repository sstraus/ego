//! RED adapter tests for the single zeroizing provider-auth lifetime.

use ego_ai::http::{Http, HttpCall};
use ego_ai::provider::{Credentials, MissingCredential, Secret};
use ego_cli::auth::{AuthEntry, AuthStore};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::tempdir;

fn oauth(access: &str) -> AuthEntry {
    AuthEntry::OAuth {
        access: access.into(),
        refresh: Some("refresh-token".into()),
        expires: Some(4_102_444_800_000),
    }
}

#[test]
fn auth_store_diagnostics_never_contain_access_or_refresh_bytes() {
    let dir = tempdir().unwrap();
    let store = AuthStore::new(dir.path());
    store.put("xai-oauth", oauth("access-token")).unwrap();
    let entries = store.read().unwrap();
    let diagnostic = format!("{entries:?}");
    assert!(!diagnostic.contains("access-token"));
    assert!(!diagnostic.contains("refresh-token"));
}

#[test]
fn auth_decode_moves_into_the_shared_zeroizing_owner() {
    let dir = tempdir().unwrap();
    let store = AuthStore::new(dir.path());
    store.put("xai-oauth", oauth("access-token")).unwrap();
    let owner = store.read_secret_material("xai-oauth").unwrap();
    assert_eq!(owner.expose_secret_bytes(), b"access-token");
}

struct NoCredentials;

impl Credentials for NoCredentials {
    fn secret(&self, name: &str) -> Result<Secret, MissingCredential> {
        Err(MissingCredential { var: name.into() })
    }
}

#[test]
fn deprecated_xai_api_key_selection_is_unavailable_before_credential_or_network() {
    struct NeverHttp;
    impl Http for NeverHttp {
        fn begin_call(&self) -> Box<dyn HttpCall> {
            panic!("deprecated xAI must refuse before transport");
        }
    }
    let result = futures::executor::block_on(ego_cli::model::resolve(
        "xai/grok-4",
        Vec::new(),
        &(Arc::new(NeverHttp) as Arc<dyn ego_ai::http::Http>),
        &NoCredentials,
        &HashMap::new(),
    ));
    assert!(matches!(
        result,
        Err(ego_ai::catalogue::ResolveError::UnknownProvider { provider, .. })
            if provider == "xai"
    ));
}
