//! Sandbox authority is committed only after an explicit host grant succeeds.
use std::sync::{Arc, Mutex};

use agent_client_protocol::{Agent, Lines};
use agent_client_protocol_schema::v1;
use ego_acp::sandbox::{AcpSandboxGrants, SandboxGrantStore};
use ego_core::permit::{CanonPath, SandboxAccess, SandboxGrant, SandboxGrantPort};
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};

#[derive(Debug, Default)]
struct Store {
    grants: Mutex<Vec<SandboxGrant>>,
    fail: bool,
}

impl SandboxGrantStore for Store {
    fn load(&self, workspace: &std::path::Path) -> Vec<SandboxGrant> {
        assert_eq!(workspace, std::path::Path::new("/workspace"));
        self.grants.lock().unwrap().clone()
    }

    fn remember<'a>(
        &'a self,
        workspace: &'a std::path::Path,
        grant: &'a SandboxGrant,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            assert_eq!(workspace, std::path::Path::new("/workspace"));
            if self.fail {
                return Err("fixture persistence refused".into());
            }
            self.grants.lock().unwrap().push(grant.clone());
            Ok(())
        })
    }
}

async fn exercise(outcome: Value, store: Option<Arc<Store>>, allowed: bool, repeats: usize) {
    let (mut to_agent, to_agent_rx) = mpsc::unbounded::<String>();
    let (from_agent, mut from_agent_rx) = mpsc::unbounded::<String>();
    let transport = Lines::new(
        from_agent.sink_map_err(std::io::Error::other),
        to_agent_rx.map(Ok::<_, std::io::Error>),
    );
    let host_outcome = outcome.clone();
    let agent = Agent
        .builder()
        .connect_with(transport, async move |connection| {
            let session = Arc::new(Mutex::new(Vec::new()));
            let adapter = AcpSandboxGrants::new(
                v1::SessionId::new("sandbox-test"),
                connection,
                "/workspace".into(),
                store
                    .clone()
                    .map(|store| store as Arc<dyn SandboxGrantStore>),
                Arc::clone(&session),
            );
            let grant = SandboxGrant::new(CanonPath::new("/outside/data"), SandboxAccess::Write);
            for _ in 0..repeats {
                assert_eq!(
                    adapter
                        .ask(&grant, "tool --output /outside/data")
                        .await
                        .allows(),
                    allowed
                );
            }
            // A permanent answer is only an intent; the root receipt must be committed first.
            let expected = if allowed && !outcome_is_durable(&outcome) {
                vec![grant]
            } else {
                Vec::new()
            };
            assert_eq!(adapter.granted(), expected);
            assert_eq!(*session.lock().unwrap(), expected);
            if let Some(store) = store {
                let persisted = store.grants.lock().unwrap();
                assert!(
                    persisted.is_empty(),
                    "the sandbox ledger must not store a new root approval"
                );
            }
            Ok(())
        });
    // Keep the response independent from the adapter's parsing policy.
    let host = async move {
        for _ in 0..repeats {
            let frame: Value = serde_json::from_str(&from_agent_rx.next().await.unwrap()).unwrap();
            assert_eq!(frame["method"], "session/request_permission");
            assert_eq!(frame["params"]["sessionId"], "sandbox-test");
            let title = frame["params"]["toolCall"]["title"].as_str().unwrap();
            assert!(title.contains("/outside/data"));
            assert!(title.contains("tool --output /outside/data"));
            let reply = if host_outcome.is_null() {
                json!({"jsonrpc":"2.0", "id":frame["id"], "error":{"code":-32603,"message":"fixture RPC failure"}})
            } else {
                json!({"jsonrpc":"2.0", "id":frame["id"], "result":{"outcome":host_outcome}})
            };
            to_agent.send(reply.to_string()).await.unwrap();
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (result, ()) = tokio::join!(agent, host);
        result.unwrap();
    })
    .await
    .expect("sandbox answer must settle");
}

fn selected(option: &str) -> Value {
    json!({"outcome":"selected", "optionId":option})
}

fn outcome_is_durable(outcome: &Value) -> bool {
    outcome["optionId"] == "sandbox-always"
}

// Catches: the ACP sandbox prompt cannot keep a refusal blocked for future sessions.
#[test]
fn sandbox_prompt_offers_a_persistent_rejection() {
    let grant = SandboxGrant::new(CanonPath::new("/outside/data"), SandboxAccess::Write);
    let request = AcpSandboxGrants::request(&v1::SessionId::new("test"), &grant, "tool", true);
    assert!(
        request
            .options
            .iter()
            .any(|option| option.kind == v1::PermissionOptionKind::RejectAlways)
    );
}

// Catches: repeated session consent duplicates grants or leaks into persistent authority.
#[tokio::test]
async fn session_consent_grants_once_without_persisting() {
    exercise(
        selected("sandbox-session"),
        Some(Arc::new(Store::default())),
        true,
        2,
    )
    .await;
}

// Catches: a permanent answer grants the sandbox before the root receipt is committed.
#[tokio::test]
async fn durable_consent_waits_for_root_receipt() {
    exercise(
        selected("sandbox-always"),
        Some(Arc::new(Store::default())),
        true,
        1,
    )
    .await;
}

// Catches: a root approval is written to the retired sandbox ledger.
#[tokio::test]
async fn durable_consent_does_not_write_sandbox_ledger() {
    exercise(
        selected("sandbox-always"),
        Some(Arc::new(Store {
            fail: true,
            ..Store::default()
        })),
        true,
        1,
    )
    .await;
}

// Catches: an unsolicited durable option grants authority when persistence is unavailable.
#[tokio::test]
async fn durable_consent_without_a_store_denies() {
    exercise(selected("sandbox-always"), None, false, 1).await;
}

// Catches: refusal, cancellation, unknown answers or RPC errors default to permission.
#[tokio::test]
async fn unanswered_and_rejected_questions_never_widen_the_sandbox() {
    for outcome in [
        selected("sandbox-deny"),
        selected("invented-allow"),
        json!({"outcome":"cancelled"}),
        Value::Null,
    ] {
        exercise(outcome, Some(Arc::new(Store::default())), false, 1).await;
    }
}
