//! CLI registry -> core request builder -> provider wire, with inert credentials.
use std::sync::Arc;

use ego_ai::catalogue::{Catalogue, ModelSlug};
use ego_ai::generator::WireGenerator;
use ego_ai::http::Http;
use ego_ai::provider::ProviderRow;
use ego_ai::wire::{
    WireFormat, openai_codex_responses::OpenAiCodexResponses,
    openai_completions::OpenAiCompletions, openai_responses::OpenAiResponses,
};
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
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::fake::FakeFs;
use ego_core::permit::{
    CanonPath, Confinement, PolicyEngine, ProcessError, ProcessExecutor, RunConfinement, SpawnSpec,
    ToolResult,
};
use ego_core::sink::NullSink;
use ego_core::state::SessionState;
use ego_core::turn::{
    CancelToken, GenerationPolicyCapture, GenerationPolicySnapshot, GenerationPolicySource,
    GenerationRuntime, Session, ToolEffectPorts, Tools, TurnConfig, TurnLoop,
};
use ego_tools::store::{JsonlStore, OsIo, SessionEvidenceStore};
use futures::future::BoxFuture;
use serde_json::Value;

use crate::ai_test_support::{Answer, FakeCredentials, NoForeign, StubServer};

struct AdmissionStore;
impl ego_core::evidence::EvidenceStore for AdmissionStore {
    fn write(
        &self,
        _: ego_core::evidence::EvidenceKey,
        _: ego_core::evidence::EvidenceRecord,
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
    fn execute<'a>(&'a self, _: &'a SpawnSpec) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(async { Err(ProcessError::Invariant("no local tools may execute".into())) })
    }
}
struct SelectedModel(Vec<GenerationProfile>);
impl GenerationModelSource for SelectedModel {
    fn resolve<'a>(
        &'a self,
        request: GenerationModelRequest,
    ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>> {
        Box::pin(async move {
            let profile = request
                .model()
                .and_then(|model| self.0.iter().find(|profile| &profile.identity == model))
                .unwrap_or(&self.0[0]);
            Ok(GenerationProfile {
                output_limit: profile.output_limit,
                identity: profile.identity.clone(),
                request_model: profile.request_model.clone(),
                thinking: profile.thinking,
                generator: profile.generator.clone(),
                capabilities: profile.capabilities.clone(),
                metering: profile.metering.clone(),
                rates: profile.rates.clone(),
                pricing_unavailable: profile.pricing_unavailable,
            })
        })
    }
}
struct FixedPolicy(Mode);
impl GenerationPolicySource for FixedPolicy {
    fn current(&self) -> Result<Arc<PolicyEngine>, String> {
        Ok(Arc::new(PolicyEngine::new(
            CanonPath::new("/ws"),
            self.0,
            Ruleset::default(),
        )))
    }
    fn capture(&self) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::new(CanonPath::new("/ws"), self.0, Ruleset::default(), None)
            .map_err(|error| error.to_string())
    }
    fn restore(
        &self,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::restore(snapshot, None).map_err(|error| error.to_string())
    }
}
#[derive(Clone, Default)]
struct UnsentRecorder(Arc<std::sync::Mutex<Vec<String>>>);
impl Http for UnsentRecorder {
    fn begin_call(&self) -> Box<dyn ego_ai::http::HttpCall> {
        Box::new(self.clone())
    }
}
impl ego_ai::http::HttpCall for UnsentRecorder {
    fn execute(
        &mut self,
        call: ego_ai::http::PreparedHttpCall,
    ) -> BoxFuture<
        'static,
        Result<ego_core::permit::OneHopResponse, ego_core::permit::TransportError>,
    > {
        let (_, _, _, request) = call.into_parts();
        self.0
            .lock()
            .unwrap()
            .push(String::from_utf8(request.body().to_vec()).unwrap());
        Box::pin(async {
            Err(ego_core::permit::TransportError::definitely_unsent(
                ego_core::permit::TransportFailure::Connect("inert unsent fixture".into()),
            ))
        })
    }
}

struct NoRetryPause;
impl ego_core::turn::RetryPause for NoRetryPause {
    fn pause(&self, _: std::time::Duration) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

async fn dispatch(
    row: ProviderRow,
    wire: &'static dyn WireFormat,
    slug: &str,
    selected: Option<Vec<String>>,
    http: Arc<dyn Http>,
    key_override: Option<&'static str>,
) -> (ExitReason, String) {
    dispatch_profiles(vec![profile(row, wire, slug, http, key_override)], selected).await
}

fn profile(
    row: ProviderRow,
    wire: &'static dyn WireFormat,
    slug: &str,
    http: Arc<dyn Http>,
    key_override: Option<&'static str>,
) -> GenerationProfile {
    // Supply ordinary model facts for a name that is not in the shipped catalogue.
    let facts_slug = if slug.ends_with("/unverified-search-model") {
        "openai/gpt-6.1-sol"
    } else {
        slug
    };
    let (_, supplied) = crate::facts::builtin_only()
        .into_iter()
        .find(|(name, _)| name == facts_slug)
        .expect("shipped ordinary model facts");
    let resolved = Catalogue::new([row])
        .with_hosted(slug, supplied)
        .resolve_hosted(&ModelSlug::parse(slug).unwrap(), wire)
        .unwrap();
    // Reuse the existing synthetic JWT credential shape from the OAuth auth
    // contract tests. It is inert test material, not a real account/token.
    let key = key_override.unwrap_or(if slug.starts_with("openai-codex/") {
        "head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig"
    } else {
        "test-key"
    });
    let credential = resolved
        .row
        .credential
        .resolve(&FakeCredentials::holding(key))
        .unwrap();
    let generator = Arc::new(WireGenerator::new(
        resolved.row,
        resolved.wire,
        http,
        credential,
    ));
    GenerationProfile {
        output_limit: None,
        identity: ModelRef(slug.into()),
        request_model: resolved.model,
        thinking: ThinkingLevel::Low,
        generator,
        capabilities: resolved.capabilities,
        metering: resolved.metering,
        rates: resolved.rates,
        pricing_unavailable: None,
    }
}

async fn dispatch_profiles(
    profiles: Vec<GenerationProfile>,
    selected: Option<Vec<String>>,
) -> (ExitReason, String) {
    dispatch_profiles_in(profiles, selected, Mode::Default).await
}

async fn dispatch_profiles_in(
    profiles: Vec<GenerationProfile>,
    selected: Option<Vec<String>>,
    mode: Mode,
) -> (ExitReason, String) {
    let fallbacks = profiles
        .iter()
        .skip(1)
        .map(|profile| profile.identity.clone())
        .collect();
    let source = Arc::new(SelectedModel(profiles));
    // Deliberately differs from the resolved route: registration must not pin
    // search to the startup model before a later model/connection selection.
    let registry = super::super::composition::tools_with_mode(
        selected.as_ref(),
        None,
        None,
        None,
        ego_core::permit::EnvPolicy::Inherited,
        Some("unsupported/startup"),
        super::super::composition::OptInTools::default(),
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let store = JsonlStore::new(root.path(), "/ws", Box::new(OsIo));
    let evidence = SessionEvidenceStore::open(root.path(), id, 1024 * 1024).unwrap();
    let fs = FakeFs::default();
    let foreign = Arc::new(NoForeign);
    let mut turn = TurnLoop::new(
        Session {
            id,
            state: SessionState::default(),
            store: &store,
            evidence: &AdmissionStore,
            clock: &FixedClock,
            sink: &NullSink,
            cancel: CancelToken::new(),
        },
        GenerationRuntime {
            source,
            policy_source: Arc::new(FixedPolicy(mode)),
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
                fallbacks,
            },
            admission: Arc::new(EvidenceAdmissionService::new()),
            invocation: RunInvocationId::new(),
            retry_pause: Arc::new(NoRetryPause),
        },
        Tools::from_registry(
            &registry,
            ToolEffectPorts::new(
                &fs,
                &NoProcess,
                &evidence,
                foreign.clone(),
                foreign.clone(),
                foreign,
            ),
            CanonPath::new("/ws"),
        ),
    )
    .unwrap();
    let exit = turn.run("prepare the request").await.unwrap().exit_reason;
    let state = turn.into_state().unwrap();
    (exit, state.errors.join("\n"))
}

#[tokio::test]
async fn default_native_search_composition_reaches_verified_api_and_oauth_without_exa() {
    // Catches: verified API/OAuth models lose default native search, explicit
    // native selection stops working, tool exclusions are ignored, or a
    // subscription default silently gets live rather than cached search.
    for oauth in [false, true] {
        for (selected, expected) in [
            (None, true),
            (Some(vec!["web_search".into()]), true),
            (Some(vec![]), false),
            (Some(vec!["read".into()]), false),
        ] {
            // Only the outgoing request is under test; a deliberately empty
            // failing transport response makes no provider-response assumption.
            let http = StubServer::new([Answer {
                status: 503,
                body: String::new(),
                headers: Vec::new(),
            }]);
            let (row, wire, slug): (_, &'static dyn WireFormat, _) = if oauth {
                (
                    ProviderRow::openai_codex_default(),
                    &OpenAiCodexResponses,
                    "openai-codex/gpt-6.1-sol",
                )
            } else {
                (
                    ProviderRow::openai(),
                    &OpenAiResponses,
                    "openai/gpt-6.1-sol",
                )
            };
            dispatch(row, wire, slug, selected.clone(), http.clone(), None).await;
            let requests = http.bodies();
            assert_eq!(requests.len(), 1, "oauth={oauth}, selected={selected:?}");
            let body: Value = serde_json::from_str(requests[0].as_deref().unwrap()).unwrap();
            let tools = body["tools"].as_array().cloned().unwrap_or_default();
            assert_eq!(
                tools.iter().any(|tool| tool["type"] == "web_search"),
                expected,
                "oauth={oauth}, selected={selected:?}, tools={tools:?}"
            );
            if oauth && expected {
                let search = tools
                    .iter()
                    .find(|tool| tool["type"] == "web_search")
                    .unwrap();
                assert_eq!(
                    search["external_web_access"],
                    Value::Bool(selected.is_some()),
                    "subscription defaults are cached; explicit selection retains live access"
                );
            }
            assert!(
                !tools
                    .iter()
                    .any(|tool| tool["type"] == "function" && tool["name"] == "web_search"),
                "native search must never become a local function"
            );
        }
    }
}

#[tokio::test]
async fn default_native_search_composition_is_the_same_in_every_permission_mode() {
    // Catches: a permission mode adds a hidden search refusal or consent branch,
    // a mode lets an explicit exclusion through, or an unknown search cost
    // strips or blocks the provider-native search declaration.
    for mode in Mode::SELECTABLE {
        for unknown_cost in [false, true] {
            for (selected, expected) in [
                (None, true),
                (Some(vec!["web_search".into()]), true),
                (Some(vec![]), false),
                (Some(vec!["read".into()]), false),
            ] {
                let http = StubServer::new([Answer {
                    status: 503,
                    body: String::new(),
                    headers: Vec::new(),
                }]);
                let mut profile = profile(
                    ProviderRow::openai(),
                    &OpenAiResponses,
                    "openai/gpt-6.1-sol",
                    http.clone(),
                    None,
                );
                if unknown_cost {
                    profile.rates = None;
                }
                dispatch_profiles_in(vec![profile], selected.clone(), mode).await;
                let requests = http.bodies();
                let context =
                    format!("mode={mode:?}, unknown_cost={unknown_cost}, selected={selected:?}");
                assert_eq!(requests.len(), 1, "{context}");
                let body: Value = serde_json::from_str(requests[0].as_deref().unwrap()).unwrap();
                let tools = body["tools"].as_array().cloned().unwrap_or_default();
                assert_eq!(
                    tools.iter().any(|tool| tool["type"] == "web_search"),
                    expected,
                    "{context}, tools={tools:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn default_native_search_composition_omits_unsupported_wire_without_fallback() {
    // Catches: admitting defaults injects native search on unsupported wires,
    // or dispatches an unconfigured local search instead.
    let http = StubServer::new([Answer {
        status: 503,
        body: String::new(),
        headers: Vec::new(),
    }]);
    dispatch(
        ProviderRow::openai(),
        &OpenAiCompletions,
        "openai/gpt-6.1-sol",
        None,
        http.clone(),
        None,
    )
    .await;
    let requests = http.bodies();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_str(requests[0].as_deref().unwrap()).unwrap();
    assert!(
        !body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["type"] == "web_search" || tool["function"]["name"] == "web_search")
    );
}

#[tokio::test]
async fn default_native_search_composition_does_not_filter_openai_model_names() {
    // Catches: newly resolved model IDs lose search until a static list is updated.
    let mut failures = Vec::new();
    for (oauth, slug) in [
        (false, "openai/unverified-search-model"),
        (true, "openai-codex/unverified-search-model"),
        (true, "openai-codex/gpt-6.2-sol"),
    ] {
        let http = StubServer::new([Answer {
            status: 503,
            body: String::new(),
            headers: Vec::new(),
        }]);
        let (row, wire): (_, &'static dyn WireFormat) = if oauth {
            (ProviderRow::openai_codex_default(), &OpenAiCodexResponses)
        } else {
            (ProviderRow::openai(), &OpenAiResponses)
        };
        dispatch(row, wire, slug, None, http.clone(), None).await;
        let requests = http.bodies();
        if requests.len() != 1 {
            failures.push(format!(
                "{slug}: {} requests instead of one",
                requests.len()
            ));
            continue;
        }
        let body: Value = serde_json::from_str(requests[0].as_deref().unwrap()).unwrap();
        let actual = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["type"] == "web_search");
        if !actual {
            failures.push(format!("{slug}: native search was not declared"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn default_native_search_composition_offers_messages_api_and_subscription_search() {
    // Catches: Anthropic search is filtered by model ID or credential type.
    for (model, key, selected, expected) in [
        ("claude-fable-5-1", "test-key", None, true),
        ("claude-fable-5-2", "test-key", None, true),
        ("claude-haiku-5-5", "test-key", None, true),
        ("claude-opus-5-5", "test-key", None, true),
        ("claude-sonnet-5-5", "test-key", None, true),
        ("claude-opus-5-5", "sk-ant-oat01-fixture", None, true),
        (
            "claude-opus-5-5",
            "sk-ant-oat01-fixture",
            Some(vec![]),
            false,
        ),
        (
            "claude-opus-5-5",
            "sk-ant-oat01-fixture",
            Some(vec!["read".into()]),
            false,
        ),
        ("unverified-search-model", "test-key", None, true),
        (
            "unverified-search-model",
            "sk-ant-oat01-fixture",
            None,
            true,
        ),
    ] {
        let http = StubServer::new([Answer {
            status: 503,
            body: String::new(),
            headers: Vec::new(),
        }]);
        dispatch(
            ProviderRow::anthropic_default(),
            &ego_ai::wire::anthropic_messages::AnthropicMessages,
            &format!("anthropic/{model}"),
            selected,
            http.clone(),
            Some(key),
        )
        .await;
        let requests = http.bodies();
        assert_eq!(requests.len(), 1, "{model} {expected}");
        let body: Value = serde_json::from_str(requests[0].as_deref().unwrap()).unwrap();
        assert_eq!(
            body["tools"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .any(|tool| tool["type"] == "web_search_20250305"),
            expected,
            "{model} {expected}"
        );
    }
}

#[tokio::test]
async fn default_native_search_composition_rechecks_each_fallback_candidate() {
    // Catches: a primary's admission is reused on a fallback at a different endpoint.
    let official = ("openai/gpt-6.1-sol", true);
    let gateway = ("openai/unverified-search-model", false);
    for candidates in [[official, gateway], [gateway, official]] {
        // Fallback requires a definitely-unsent failure. A received 400 is
        // not equivalent: the provider may already have performed search.
        let http = Arc::new(UnsentRecorder::default());
        let profiles = candidates
            .iter()
            .map(|&(slug, official)| {
                let row = if official {
                    ProviderRow::openai()
                } else {
                    ProviderRow::openai_at("https://gateway.example.test/v1/".parse().unwrap())
                };
                profile(row, &OpenAiResponses, slug, http.clone(), None)
            })
            .collect();
        dispatch_profiles(profiles, None).await;
        let requests = http.0.lock().unwrap().clone();
        assert_eq!(requests.len(), 2, "{candidates:?}");
        for (request, (slug, expected)) in requests.iter().zip(candidates) {
            let body: Value = serde_json::from_str(request).unwrap();
            assert_eq!(body["model"], slug.split_once('/').unwrap().1);
            let search = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["type"] == "web_search");
            assert_eq!(search, expected, "{candidates:?} {slug}");
            assert!(
                !body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == "web_search")
            );
        }
    }
}

#[tokio::test]
async fn default_native_search_composition_offers_xai_search_and_honors_exclusions() {
    // Catches: an xAI model-name gate or an ignored explicit tool exclusion.
    use ego_ai::wire::xai_responses::XaiResponses;

    for (slug, selected, expected) in [
        ("xai-oauth/grok-4.7", None, true),
        ("xai-oauth/grok-4.7", Some(vec!["web_search".into()]), true),
        ("xai-oauth/grok-4.7", Some(vec![]), false),
        ("xai-oauth/grok-4.7", Some(vec!["read".into()]), false),
        ("xai-oauth/unverified-search-model", None, true),
    ] {
        let http = StubServer::new([Answer {
            status: 503,
            body: String::new(),
            headers: Vec::new(),
        }]);
        dispatch(
            ProviderRow::xai_oauth_default(),
            &XaiResponses,
            slug,
            selected.clone(),
            http.clone(),
            None,
        )
        .await;
        let requests = http.bodies();
        assert_eq!(requests.len(), 1, "{slug} {selected:?}");
        let body: Value = serde_json::from_str(requests[0].as_deref().unwrap()).unwrap();
        let tools = body["tools"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            tools.iter().any(|tool| tool["type"] == "web_search"),
            expected,
            "{slug} {selected:?}"
        );
        assert!(!tools.iter().any(|tool| tool["name"] == "web_search"));
    }
}

#[tokio::test]
async fn default_native_search_composition_does_not_advertise_google_grounding() {
    // Catches: Google grounding is exposed before its display and replay requirements are met.
    use ego_ai::wire::gemini_interactions::GeminiInteractions;

    for selected in [None, Some(vec!["web_search".into()])] {
        let http = StubServer::new([Answer {
            status: 503,
            body: String::new(),
            headers: Vec::new(),
        }]);
        dispatch(
            ProviderRow::gemini_default(),
            &GeminiInteractions,
            "gemini/gemini-3.8-flash",
            selected.clone(),
            http.clone(),
            None,
        )
        .await;
        let requests = http.bodies();
        assert_eq!(requests.len(), 1, "tools={selected:?}");
        let body: Value = serde_json::from_str(requests[0].as_deref().unwrap()).unwrap();
        assert!(
            !body["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|tool| tool["type"] == "google_search" || tool["name"] == "web_search"),
            "tools={selected:?}"
        );
    }
}

#[tokio::test]
async fn default_native_search_composition_keeps_custom_gateway_opt_in() {
    // Catches: a compatible gateway is silently treated as verified native search.
    for (selected, expected) in [(None, false), (Some(vec!["web_search".into()]), true)] {
        let custom = "https://gateway.example.test/v1/";
        let cases: Vec<(_, &'static dyn WireFormat, _)> = vec![
            (
                ProviderRow::openai_at(custom.parse().unwrap()),
                &OpenAiResponses,
                "openai/gpt-6.1-sol",
            ),
            // Exercise the subscription encoder against a supplied custom
            // base; this is request preparation, not OAuth account acceptance.
            (
                ProviderRow::openai_at(custom.parse().unwrap()),
                &OpenAiCodexResponses,
                "openai/gpt-6.1-sol",
            ),
            (
                ProviderRow::anthropic(custom.parse().unwrap()),
                &ego_ai::wire::anthropic_messages::AnthropicMessages,
                "anthropic/claude-opus-5-5",
            ),
        ];
        for (row, wire, slug) in cases {
            let http = StubServer::new([Answer {
                status: 503,
                body: String::new(),
                headers: Vec::new(),
            }]);
            dispatch(row, wire, slug, selected.clone(), http.clone(), None).await;
            let requests = http.bodies();
            assert_eq!(requests.len(), 1, "{slug}");
            let body: Value = serde_json::from_str(requests[0].as_deref().unwrap()).unwrap();
            assert_eq!(
                body["tools"].as_array().unwrap().iter().any(|tool| {
                    tool["type"] == "web_search" || tool["type"] == "web_search_20250305"
                }),
                expected,
                "{slug} {selected:?}"
            );
        }
    }
}

#[tokio::test]
async fn default_native_search_composition_keeps_offline_remote_inference_unsent() {
    // Catches: default native search bypasses the actual offline HTTP boundary.
    for oauth in [false, true] {
        let confinement = RunConfinement::new(
            Confinement::Off,
            true,
            CanonPath::new(crate::ai_test_support::native("/ws")),
        )
        .unwrap();
        let http = Arc::new(crate::provider_http::ProviderHttpAdapter::inference(
            Arc::new(ego_tools::http::InferenceHttpPorts::new(
                confinement,
                CancelToken::new(),
                ego_tools::exec::HttpCallTimeout::default(),
            )),
        ));
        let (row, wire, slug): (_, &'static dyn WireFormat, _) = if oauth {
            (
                ProviderRow::openai_codex_default(),
                &OpenAiCodexResponses,
                "openai-codex/gpt-6.1-sol",
            )
        } else {
            (
                ProviderRow::openai(),
                &OpenAiResponses,
                "openai/gpt-6.1-sol",
            )
        };
        let (exit, state) =
            dispatch(row, wire, slug, Some(vec!["web_search".into()]), http, None).await;
        assert_ne!(exit, ExitReason::Completed, "oauth={oauth}");
        assert!(
            state.to_lowercase().contains("offline"),
            "oauth={oauth}: {state}"
        );
    }
}
