//! adapter tests for the single zeroizing provider-auth lifetime.

mod support;

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

/// regression case: what the ACP host is handed for a pre-turn catalogue refusal.
///
/// `error.data` is the field a host puts in front of a human, and it carries
/// `error.to_string()` of whatever failed — so before this story a provider
/// that echoed the operator's credential into its error document had that
/// echoed onto another program's wire, out of ego's process entirely.
///
/// This drives the real `ego acp` binary against the stub with `/api/tags`
/// refusing, which is the pre-turn half: no generation is ever requested. The
/// answer must still be actionable — it names the endpoint and the status —
/// and must contain none of the bytes the endpoint sent.
#[test]
fn an_acp_host_is_handed_the_status_of_a_refused_catalogue_and_none_of_its_body() {
    use support::{Ego as Harness, Plan, Stub};

    const ECHOED: &str = r#"{"error":"upstream: authorization Bearer acp-catalogue-secret","marker":"acp-body-secret"}"#;

    let stub = Stub::start(Plan {
        tags_refusal: Some((503, ECHOED.to_string())),
        ..Plan::default()
    });
    let ego = Harness::against(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nsandbox = \"off\"\n",
    );
    let workspace = ego.workspace_path().to_owned();
    let running = Harness::at(ego.home_path(), &workspace, Some(&stub))
        .subcommand("acp")
        .spawn_conversing();
    let mut host = support::AcpHost::adopt(running);
    host.initialize();
    let cwd = workspace.to_str().expect("a utf-8 workspace").to_owned();
    let session = host.new_session(&cwd);

    let answered = host.request(
        "session/prompt",
        serde_json::json!({
            "sessionId": session,
            "prompt": [{ "type": "text", "text": "Ask" }],
        }),
    );
    // The message and `data` together: a refused catalogue is a known
    // failure since regression, so its cause is in the message and there
    // is no executor diagnostic to repeat in `data`.
    let error = answered
        .get("error")
        .unwrap_or_else(|| panic!("a refused catalogue did not fail the prompt: {answered}"));
    let data = format!(
        "{} {}",
        error["message"].as_str().unwrap_or_default(),
        error["data"].as_str().unwrap_or_default()
    );
    assert!(
        !data.contains("uncertain"),
        "a refused catalogue left the run uncertain: {answered}"
    );

    assert!(
        data.contains("api/tags"),
        "error.data does not name the endpoint that refused: {data}"
    );
    assert!(
        data.contains("503"),
        "error.data does not carry the status: {data}"
    );
    for secret in ["acp-catalogue-secret", "acp-body-secret", "marker"] {
        assert!(
            !data.contains(secret),
            "error.data relayed the endpoint's body ({secret}): {data}"
        );
    }
    host.finish();
}
