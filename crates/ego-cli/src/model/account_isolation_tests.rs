//! Configured-instance credential identity at the real generation boundary.

use super::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use ego_ai::http::{Http, HttpCall, PreparedHttpCall};
use ego_core::budget::{BudgetGate, MeteringIdentity, SettlementSource};
use ego_core::config::{
    ConnectionAccess, ConnectionConfig, ProviderInstance, ProviderInstanceModel,
    ProviderModelContract, ResolvedProviders, Source, Sourced,
};
use ego_core::cost::CurrencyCode;
use ego_core::event::id::GenerationId;
use ego_core::generator::attempt::GenerationAttempt;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, ErrorKind, GenRequest, Message, ReasoningState, Role,
};
use ego_core::permit::{OneHopResponse, TransportError, TransportFailure};
use futures::StreamExt;
use futures::future::BoxFuture;

#[derive(Clone, Default)]
struct RefusingTransport(Arc<AtomicUsize>);

impl Http for RefusingTransport {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(self.clone())
    }
}

impl HttpCall for RefusingTransport {
    fn execute(
        &mut self,
        _call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        // This is our transport port refusing an effect, not provider wire data.
        Box::pin(async {
            Err(TransportError::definitely_unsent(
                TransportFailure::Exchange("test-owned transport refusal".into()),
            ))
        })
    }
}

fn configuration() -> (ResolvedProviders, BTreeMap<String, ConnectionConfig>) {
    let facts = crate::facts::builtin_only()
        .into_iter()
        .find(|(slug, _)| slug == "anthropic/claude-opus-5-5")
        .unwrap()
        .1
        .capabilities;
    let mut providers = ResolvedProviders::default();
    let mut connections = BTreeMap::new();
    for (id, key) in [
        ("account_a", "INSTANCE_A_API_KEY"),
        ("account_b", "INSTANCE_B_API_KEY"),
    ] {
        providers.instances.insert(
            id.parse().unwrap(),
            Sourced {
                source: Source::User,
                value: ProviderInstance {
                    profile: "anthropic".parse().unwrap(),
                    base_url: "https://instance.example/v1/".parse().unwrap(),
                    credential_ref: Some(id.parse().unwrap()),
                    models: BTreeMap::from([(
                        "assistant".parse().unwrap(),
                        ProviderInstanceModel {
                            upstream_id: "claude-opus-5-5".into(),
                            contract: ProviderModelContract::Messages,
                            facts: facts.clone(),
                            metering: MeteringIdentity::Currency {
                                settlement: SettlementSource::Quote,
                            },
                        },
                    )]),
                },
            },
        );
        connections.insert(
            id.into(),
            ConnectionConfig {
                name: id.into(),
                vendor: "anthropic".into(),
                access: ConnectionAccess::Key,
                key_env: Some(key.into()),
                azure_resource: None,
            },
        );
    }
    (providers, connections)
}

// Catches: instance resolution tags every generator as unbound, accepting
// unbound private history and rejecting its own account-tagged history; also
// protects A -> B model switches and fresh resolution after credential_ref edits.
#[tokio::test]
async fn instances_refuse_foreign_or_unbound_private_history_before_dispatch() {
    const CHILD: &str = "EGO_INSTANCE_PRIVATE_HISTORY_CHILD";
    const TEST: &str = "model::account_isolation_tests::instances_refuse_foreign_or_unbound_private_history_before_dispatch";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home.path())
            .env("EGO_HOME", home.path())
            .env("INSTANCE_A_API_KEY", "synthetic-instance-a-key")
            .env("INSTANCE_B_API_KEY", "synthetic-instance-b-key")
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("test {TEST} ... ok")));
        return;
    }
    let (mut providers, connections) = configuration();
    let http = Arc::new(RefusingTransport::default());
    let source = Arc::new(
        CliGenerationModelSource::new(
            ModelRef("account_a/assistant".into()),
            Some(ThinkingLevel::Low),
            None,
            Ceilings::default(),
            Default::default(),
        )
        .with_connections(None, connections.clone())
        .with_provider_instances(providers.clone()),
    );
    let bound = source.clone().bind(http.clone(), CancelToken::new(), None);
    let first = bound
        .resolve(GenerationModelRequest::new(GenerationRole::Main))
        .await
        .unwrap();
    source
        .selection()
        .install(ModelRef("account_b/assistant".into()), ThinkingLevel::Low)
        .unwrap();
    let switched = bound
        .resolve(GenerationModelRequest::new(GenerationRole::Main))
        .await
        .unwrap();
    providers
        .instances
        .get_mut(&"account_a".parse().unwrap())
        .unwrap()
        .value
        .credential_ref = Some("account_b".parse().unwrap());
    let resumed = Arc::new(
        CliGenerationModelSource::new(
            ModelRef("account_a/assistant".into()),
            Some(ThinkingLevel::Low),
            None,
            Ceilings::default(),
            Default::default(),
        )
        .with_connections(None, connections)
        .with_provider_instances(providers),
    )
    .bind(http.clone(), CancelToken::new(), None)
    .resolve(GenerationModelRequest::new(GenerationRole::Main))
    .await
    .unwrap();
    let mut failures = Vec::new();
    for (boundary, account, profile) in [
        ("first A", "account_a", first),
        ("switch to B", "account_b", switched),
        ("fresh A alias rebound to B", "account_b", resumed),
    ] {
        for origin in [Some("account_a"), Some("account_b"), None] {
            // Owned saved-history input, not a claimed provider response. The
            // body is deliberately opaque JSON; this test asserts no vendor
            // acceptance or signature semantics, only our dispatch authority.
            let request = GenRequest {
                model: profile.request_model.clone(),
                messages: Conversation::new(vec![
                    Message::text(Role::User, "previous question"),
                    Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Reasoning {
                            state: ReasoningState {
                                connection: origin.map(str::to_owned),
                                dialect: "anthropic-messages".into(),
                                blob: "{}".into(),
                            },
                        }],
                    },
                    Message::text(Role::User, "continue"),
                ])
                .unwrap(),
                tools: vec![],
                thinking: profile.thinking,
                max_output_tokens: Some(2048),
                parallel_tool_calls: false,
                cache: None,
                web_search: None,
            };
            let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").unwrap());
            let reservation = gate
                .admit(&profile.metering, None, &request, &profile.capabilities)
                .unwrap();
            let attempt = GenerationAttempt::new(
                GenerationId::new(),
                reservation,
                &profile.metering,
                None,
                &request,
                &profile.capabilities,
            )
            .unwrap();
            let before = http.0.load(Ordering::SeqCst);
            let events: Vec<_> = profile.generator.generate(request, attempt).collect().await;
            let calls = http.0.load(Ordering::SeqCst) - before;
            let refused = events.iter().any(|event| matches!(event, AiEvent::Error { kind: ErrorKind::Invalid, message } if message.contains("private provider history belongs to another or unbound connection")));
            let allowed = origin == Some(account);
            if calls != usize::from(allowed) || refused == allowed {
                failures.push(format!("{boundary}, origin={origin:?}: calls={calls}, refused={refused}, expected_allowed={allowed}; events={events:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
