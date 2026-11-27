//! Builtin xAI capabilities through real next-request preparation, without network.
use std::sync::Arc;

use ego_ai::catalogue::{Catalogue, ModelSlug};
use ego_ai::generator::WireGenerator;
use ego_ai::provider::ProviderRow;
use ego_ai::wire::xai_responses::XaiResponses;
use ego_core::budget::{BudgetGate, SharedBudgetGate};
use ego_core::config::{Mode, ModelRef, Ruleset};
use ego_core::cost::{CurrencyCode, Money};
use ego_core::event::domain::ExitReason;
use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{EvidenceAdmissionService, RunInvocationId};
use ego_core::generation_model_source::{
    GenerationModelError, GenerationModelRequest, GenerationModelSource, GenerationProfile,
};
use ego_core::generator::history::conversation_from;
use ego_core::generator::port::Generator;
use ego_core::generator::types::{ContentBlock, GenRequest, ThinkingLevel};
use ego_core::permit::fake::FakeFs;
use ego_core::permit::{
    CanonPath, PolicyEngine, ProcessError, ProcessExecutor, SpawnSpec, ToolResult,
};
use ego_core::sink::NullSink;
use ego_core::state::SessionState;
use ego_core::turn::{
    CancelToken, GenerationPolicyCapture, GenerationPolicySnapshot, GenerationPolicySource,
    GenerationRuntime, Session, ToolEffectPorts, Tools, TurnConfig, TurnLoop,
};
use ego_tools::store::{JsonlStore, OsIo, SessionEvidenceStore};
use futures::future::BoxFuture;
use serde_json::{Value, json};

// Reuse inert credentials/effect ports and an in-memory HTTP port, not a live provider.
use crate::ai_test_support::{Answer, FakeCredentials, NoForeign, StubServer};

// Session admission evidence and tool/blob evidence are distinct ports.
// This regression does not inspect admission receipts; keep that port inert.
struct AdmissionStore;
impl ego_core::evidence::EvidenceStore for AdmissionStore {
    fn write(
        &self,
        _key: ego_core::evidence::EvidenceKey,
        _record: ego_core::evidence::EvidenceRecord,
    ) -> Result<ego_core::evidence::EvidenceWrite, ego_core::evidence::EvidenceError> {
        Ok(ego_core::evidence::EvidenceWrite::Published)
    }
}

struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(1_787_000_000_000)
    }
}

struct NoProcess;
impl ProcessExecutor for NoProcess {
    fn execute<'a>(
        &'a self,
        _spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(async {
            Err(ProcessError::Invariant(
                "no process tools in this test".into(),
            ))
        })
    }
}

struct SelectedModel(GenerationProfile);
impl GenerationModelSource for SelectedModel {
    fn resolve<'a>(
        &'a self,
        _request: GenerationModelRequest,
    ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>> {
        Box::pin(async {
            Ok(GenerationProfile {
                output_limit: self.0.output_limit,
                identity: self.0.identity.clone(),
                request_model: self.0.request_model.clone(),
                thinking: self.0.thinking,
                generator: self.0.generator.clone(),
                capabilities: self.0.capabilities.clone(),
                metering: self.0.metering.clone(),
                rates: self.0.rates.clone(),
                pricing_unavailable: self.0.pricing_unavailable,
            })
        })
    }
}

struct NoRetryPause;
impl ego_core::turn::RetryPause for NoRetryPause {
    fn pause(&self, _delay: std::time::Duration) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

struct FixedPolicy;
impl GenerationPolicySource for FixedPolicy {
    fn current(&self) -> Result<Arc<PolicyEngine>, String> {
        Ok(Arc::new(PolicyEngine::new(
            CanonPath::new("/ws"),
            Mode::Default,
            Ruleset::default(),
        )))
    }
    fn capture(&self) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::new(
            CanonPath::new("/ws"),
            Mode::Default,
            Ruleset::default(),
            None,
        )
        .map_err(|error| error.to_string())
    }
    fn restore(
        &self,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::restore(snapshot, None).map_err(|error| error.to_string())
    }
}

fn frame(value: Value) -> String {
    format!("data: {value}\n\n")
}

#[tokio::test]
async fn builtin_xai_next_request_keeps_own_private_search_and_reasoning() {
    // Catches: the shipped builtin facts strip new xAI private items before the
    // next main request, despite a working direct decoder/serializer round trip.
    let slug = ModelSlug::parse("xai-oauth/grok-4.7").unwrap();
    let (_, supplied) = super::builtin_only()
        .into_iter()
        .find(|(name, _)| name == "xai-oauth/grok-4.7")
        .expect("shipped builtin model");
    let resolved = Catalogue::new([ProviderRow::xai_oauth_default()])
        .with_hosted(&slug.to_string(), supplied)
        .resolve_hosted(&slug, &XaiResponses)
        .unwrap();
    let capabilities = resolved.capabilities.clone();
    // Published shared item shape, then source-derived xAI reasoning/envelope inputs.
    // These are not claimed to be xAI live captures or valid provider ciphertext.
    let output: Vec<Value> = serde_json::from_str(include_str!(
        "../../../ego-ai/tests/goldens/hosted-search/openai-output.json"
    ))
    .unwrap();
    let reasoning = json!({"type":"reasoning","id":"reasoning_contract","encrypted_content":"test-ciphertext","summary":[]});
    let mut first = String::new();
    for item in [&output[0], &reasoning, &output[1]] {
        first.push_str(&frame(
            json!({"type":"response.output_item.done","item":item}),
        ));
    }
    first.push_str(&frame(json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":20}}})));
    let second = frame(
        json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":1}}}),
    );
    let http = StubServer::new([first, second].into_iter().map(|body| Answer {
        status: 200,
        body,
        headers: Vec::new(),
    }));
    let credential = resolved
        .row
        .credential
        .resolve(&FakeCredentials::holding("test-key"))
        .unwrap();
    let generator = Arc::new(
        WireGenerator::new(
            resolved.row.clone(),
            resolved.wire,
            http.clone(),
            credential,
        )
        .with_connection_id(Some("account-a".into())),
    );
    let source = Arc::new(SelectedModel(GenerationProfile {
        output_limit: None,
        identity: ModelRef(slug.to_string()),
        request_model: resolved.model.clone(),
        thinking: ThinkingLevel::Low,
        generator: generator.clone(),
        capabilities: capabilities.clone(),
        metering: resolved.metering,
        rates: resolved.rates,
        pricing_unavailable: None,
    }));
    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let store = JsonlStore::new(root.path(), "/ws", Box::new(OsIo));
    let evidence = SessionEvidenceStore::open(root.path(), session, 1024 * 1024).unwrap();
    let clock = FixedClock;
    let fs = FakeFs::default();
    let process = NoProcess;
    let foreign = Arc::new(NoForeign);
    let registry: Vec<Arc<dyn ego_core::permit::Tool>> = vec![Arc::new(ego_core::permit::Typed(
        ego_tools::tools::HostedWebSearch,
    ))];
    let mut turn = TurnLoop::new(
        Session {
            id: session,
            state: SessionState::default(),
            store: &store,
            evidence: &AdmissionStore,
            clock: &clock,
            sink: &NullSink,
            cancel: CancelToken::new(),
        },
        GenerationRuntime {
            source,
            policy_source: Arc::new(FixedPolicy),
            gate: SharedBudgetGate::new(BudgetGate::new(Money::new(
                "10".parse().unwrap(),
                CurrencyCode::new("USD").unwrap(),
            ))),
            config: TurnConfig {
                cache: ego_core::generator::capabilities::CacheRetention::None,
                system: None,
                identity: None,
                token_ceiling: None,
                retry: 0,
                fallbacks: Vec::new(),
            },
            admission: Arc::new(EvidenceAdmissionService::new()),
            invocation: RunInvocationId::new(),
            retry_pause: Arc::new(NoRetryPause),
        },
        Tools::new(
            &registry,
            ToolEffectPorts::new(
                &fs,
                &process,
                &evidence,
                foreign.clone(),
                foreign.clone(),
                foreign,
            ),
            CanonPath::new("/ws"),
        ),
    )
    .unwrap();
    assert_eq!(
        turn.run("first answer").await.unwrap().exit_reason,
        ExitReason::Completed
    );
    assert_eq!(
        turn.run("continue the same account")
            .await
            .unwrap()
            .exit_reason,
        ExitReason::Completed
    );
    let state = turn.into_state().unwrap();
    let requests = http.bodies();
    assert_eq!(
        requests.len(),
        2,
        "both real main requests must reach the wire"
    );
    let first: Value = serde_json::from_str(requests[0].as_deref().unwrap()).unwrap();
    assert_eq!(first["tools"], json!([{"type":"web_search"}]));
    let next: Value = serde_json::from_str(requests[1].as_deref().unwrap()).unwrap();
    let private: Vec<_> = next["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("web_search_call" | "reasoning" | "message")
            )
        })
        .cloned()
        .collect();
    assert_eq!(
        private,
        vec![output[0].clone(), reasoning, output[1].clone()]
    );
    let (messages, _) = conversation_from(&state).unwrap();
    let blocks: Vec<_> = messages
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| {
            matches!(
                block,
                ContentBlock::ProviderContent { .. } | ContentBlock::Reasoning { .. }
            )
        })
        .cloned()
        .collect();
    assert_eq!(blocks.len(), 3);
    for block in &blocks {
        let mut foreign = block.clone();
        let state = match &mut foreign {
            ContentBlock::ProviderContent { state, .. } | ContentBlock::Reasoning { state } => {
                state
            }
            _ => unreachable!(),
        };
        assert_eq!(state.connection.as_deref(), Some("account-a"));
        state.dialect = "foreign-responses".into();
        assert!(
            !capabilities.accepts_block(&foreign),
            "do not broaden the sanitizer"
        );
    }
    let replay = GenRequest {
        cache: None,
        web_search: None,
        model: resolved.model,
        messages,
        tools: Vec::new(),
        thinking: ThinkingLevel::Low,
        max_output_tokens: Some(1024),
        parallel_tool_calls: false,
    };
    generator.ensure_expressible(&replay).unwrap();
    let other_credential = resolved
        .row
        .credential
        .resolve(&FakeCredentials::holding("test-key"))
        .unwrap();
    let other = WireGenerator::new(resolved.row, &XaiResponses, http.clone(), other_credential)
        .with_connection_id(Some("account-b".into()));
    assert!(
        other.ensure_expressible(&replay).is_err(),
        "same dialect must not cross accounts"
    );
    assert_eq!(
        http.bodies().len(),
        2,
        "expressibility cannot dispatch HTTP"
    );
}
