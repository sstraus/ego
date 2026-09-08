use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ego_ai::catalogue::{
    FactFreshness, ModelAvailability, ModelAvailabilityCause, ModelCatalogueEntry,
    ModelCatalogueSnapshot, ModelFactProvenance, ModelSlug, SuppliedAuthority,
};
use ego_ai::http::{Http, HttpCall, PreparedHttpCall};
use ego_ai::provider::{CredentialStrategy, ProviderRow};
use ego_core::config::{
    ModelRef, UserConfigError, UserConfigMutation, UserConfigMutationResult, UserConfigPublication,
    UserConfigSnapshot, UserConfigStore, UserConfigTarget,
};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::{OneHopResponse, TransportError};
use ego_core::public::MeteringKind;
use futures::future::BoxFuture;
use url::Url;

use super::{
    EnvironmentPresence, StartupDecision, credential_present, decide_missing_model,
    discover_ollama, persist_startup_selection,
};

#[derive(Default)]
struct Presence {
    nonblank: BTreeSet<String>,
    set: BTreeSet<String>,
    queried: Mutex<Vec<String>>,
}

impl Presence {
    fn with_nonblank(names: &[&str]) -> Self {
        Self {
            nonblank: names.iter().map(|name| (*name).to_owned()).collect(),
            ..Self::default()
        }
    }

    fn queried(&self) -> Vec<String> {
        self.queried.lock().expect("presence fixture lock").clone()
    }
}

impl EnvironmentPresence for Presence {
    fn has_nonblank(&self, name: &str) -> bool {
        self.queried
            .lock()
            .expect("presence fixture lock")
            .push(name.to_owned());
        self.nonblank.contains(name)
    }

    fn is_set(&self, name: &str) -> bool {
        self.queried
            .lock()
            .expect("presence fixture lock")
            .push(name.to_owned());
        self.set.contains(name)
    }
}

#[test]
fn credential_presence_exhausts_strategies_without_commands_or_secret_values() {
    let environment = Presence::with_nonblank(&["BEARER", "HEADER", "FALLBACK", "API_KEY"]);
    let observed = BTreeSet::from(["oauth-provider".to_owned(), "anthropic".to_owned()]);

    assert!(credential_present(
        &CredentialStrategy::None,
        &environment,
        &observed
    ));
    assert!(credential_present(
        &CredentialStrategy::BearerFromEnv {
            var: "BEARER".to_owned(),
        },
        &environment,
        &observed
    ));
    assert!(!credential_present(
        &CredentialStrategy::BearerFromEnv {
            var: "MISSING".to_owned(),
        },
        &environment,
        &observed
    ));
    assert!(credential_present(
        &CredentialStrategy::HeaderFromEnv {
            header: "x-api-key".to_owned(),
            var: "HEADER".to_owned(),
        },
        &environment,
        &observed
    ));
    assert!(credential_present(
        &CredentialStrategy::OAuth {
            provider: "oauth-provider".to_owned(),
        },
        &environment,
        &observed
    ));
    assert!(!credential_present(
        &CredentialStrategy::OAuth {
            provider: "unobserved".to_owned(),
        },
        &environment,
        &observed
    ));
    assert!(credential_present(
        &CredentialStrategy::OAuthOrEnv {
            provider: "unobserved".to_owned(),
            var: "FALLBACK".to_owned(),
        },
        &environment,
        &observed
    ));
    assert!(!credential_present(
        &CredentialStrategy::OAuthOrEnv {
            provider: "unobserved".to_owned(),
            var: "MISSING".to_owned(),
        },
        &environment,
        &observed
    ));
    assert!(credential_present(
        &CredentialStrategy::Anthropic {
            provider: "unobserved".to_owned(),
            api_key_var: "API_KEY".to_owned(),
            oauth_var: "OAUTH".to_owned(),
        },
        &environment,
        &observed
    ));
    assert!(credential_present(
        &CredentialStrategy::Anthropic {
            provider: "anthropic".to_owned(),
            api_key_var: "MISSING".to_owned(),
            oauth_var: "MISSING".to_owned(),
        },
        &environment,
        &observed
    ));
    assert!(!credential_present(
        &CredentialStrategy::Anthropic {
            provider: "unobserved".to_owned(),
            api_key_var: "MISSING".to_owned(),
            oauth_var: "MISSING".to_owned(),
        },
        &environment,
        &observed
    ));
    assert_eq!(
        environment.queried(),
        vec![
            "BEARER", "MISSING", "HEADER", "FALLBACK", "MISSING", "API_KEY", "MISSING", "MISSING",
        ]
    );
}

fn entry(slug: &str, availability: ModelAvailability) -> ModelCatalogueEntry {
    ModelCatalogueEntry {
        slug: ModelSlug::parse(slug).expect("a provider/model slug"),
        capabilities: ModelCapabilities {
            schema_version: ModelCapabilities::SCHEMA_VERSION,
            max_context_tokens: None,
            max_output_tokens: None,
            max_tools: None,
            supports_parallel_tool_calls: false,
            reasoning_carryover: ReasoningCarryover::Unsupported,
            reasoning_efforts: ReasoningEffortSupport::Any,
            reasoning_state_dialect: None,
            cancel: CancelSemantics::Unsupported,
            cache: CacheRetention::None,
            transport: Transport::Sse,
            image_input: ImageInputSupport::Unknown,
        },
        metering: MeteringKind::Unmetered,
        provenance: ModelFactProvenance::supplied(SuppliedAuthority::Builtin),
        freshness: FactFreshness::Current,
        availability,
    }
}

fn snapshot(entries: Vec<ModelCatalogueEntry>) -> Arc<ModelCatalogueSnapshot> {
    Arc::new(ModelCatalogueSnapshot::from_entries_and_sources(
        entries,
        Vec::new(),
    ))
}

fn defaults(entries: &[(&str, &str)]) -> BTreeMap<String, ModelSlug> {
    entries
        .iter()
        .map(|(provider, slug)| {
            (
                (*provider).to_owned(),
                ModelSlug::parse(slug).expect("a default model slug"),
            )
        })
        .collect()
}

#[test]
fn missing_model_decisions_cover_zero_one_multiple_and_bad_defaults() {
    let zero = decide_missing_model(
        snapshot(Vec::new()),
        &BTreeMap::new(),
        &BTreeSet::new(),
        ThinkingLevel::Medium,
        false,
    );
    let StartupDecision::Headless(error) = zero else {
        panic!("zero providers must refuse headless startup")
    };
    assert!(error.to_string().contains("--model <provider/model>"));
    assert!(error.to_string().contains("config set model"));

    let one_snapshot = snapshot(vec![entry(
        "openai/gpt-5.6-sol",
        ModelAvailability::Available,
    )]);
    let one = decide_missing_model(
        Arc::clone(&one_snapshot),
        &defaults(&[("openai", "openai/gpt-5.6-sol")]),
        &BTreeSet::from(["openai".to_owned()]),
        ThinkingLevel::Medium,
        false,
    );
    let StartupDecision::Headless(error) = one else {
        panic!("one provider must still refuse headless startup")
    };
    assert!(error.to_string().contains("openai/gpt-5.6-sol"));

    let multiple = decide_missing_model(
        one_snapshot,
        &defaults(&[("openai", "openai/gpt-5.6-sol")]),
        &BTreeSet::from(["anthropic".to_owned(), "openai".to_owned()]),
        ThinkingLevel::Medium,
        true,
    );
    assert!(matches!(multiple, StartupDecision::Pick(_, None)));

    let missing_default = decide_missing_model(
        snapshot(vec![entry(
            "openai/gpt-5.6-sol",
            ModelAvailability::Available,
        )]),
        &BTreeMap::new(),
        &BTreeSet::from(["openai".to_owned()]),
        ThinkingLevel::Medium,
        true,
    );
    let StartupDecision::Pick(_, Some(error)) = missing_default else {
        panic!("one provider without a default must explain the picker")
    };
    assert!(error.to_string().contains("no startup default"));

    let unavailable_default = decide_missing_model(
        snapshot(vec![
            entry("openai/gpt-5.6-sol", ModelAvailability::Available),
            entry(
                "openai/gpt-4-retired",
                ModelAvailability::Unavailable {
                    cause: ModelAvailabilityCause::MappingUnavailable,
                    message: "retired fixture row".to_owned(),
                },
            ),
        ]),
        &defaults(&[("openai", "openai/gpt-4-retired")]),
        &BTreeSet::from(["openai".to_owned()]),
        ThinkingLevel::Medium,
        true,
    );
    let StartupDecision::Pick(_, Some(error)) = unavailable_default else {
        panic!("an unavailable default must explain the picker")
    };
    assert!(error.to_string().contains("cannot be selected"));
}

#[test]
fn sole_provider_default_preserves_an_explicit_off_thinking_level() {
    let decision = decide_missing_model(
        snapshot(vec![entry("ollama/qwen3", ModelAvailability::Available)]),
        &defaults(&[("ollama", "ollama/qwen3")]),
        &BTreeSet::from(["ollama".to_owned()]),
        ThinkingLevel::Off,
        true,
    );
    let StartupDecision::AutoSelect(request) = decision else {
        panic!("a valid sole provider default should auto-select")
    };
    assert_eq!(request.target(), ModelRef("ollama/qwen3".to_owned()));
    assert_eq!(request.thinking(), ThinkingLevel::Off);
}

struct RecordingStore {
    outcome: Mutex<Result<UserConfigMutationResult, UserConfigError>>,
    mutations: Mutex<Vec<UserConfigMutation>>,
}

impl RecordingStore {
    fn with_outcome(outcome: Result<UserConfigPublication, UserConfigError>) -> Self {
        let mutation = UserConfigMutation::ModelAndThinking {
            model: "ollama/qwen3".to_owned(),
            thinking: ThinkingLevel::Off,
        };
        Self {
            outcome: Mutex::new(outcome.map(|publication| UserConfigMutationResult {
                target: UserConfigTarget::Base,
                facts: mutation.facts(),
                changed: true,
                publication,
            })),
            mutations: Mutex::new(Vec::new()),
        }
    }
}

impl UserConfigStore for RecordingStore {
    fn snapshot(&self, _target: UserConfigTarget) -> Result<UserConfigSnapshot, UserConfigError> {
        Err(UserConfigError::new("not used"))
    }

    fn rule_snapshot(
        &self,
        _target: UserConfigTarget,
    ) -> Result<ego_core::config::RuleSnapshot, UserConfigError> {
        Err(UserConfigError::new("not used"))
    }

    fn mutate(
        &self,
        mutation: UserConfigMutation,
    ) -> Result<UserConfigMutationResult, UserConfigError> {
        self.mutations
            .lock()
            .expect("store fixture lock")
            .push(mutation);
        self.outcome.lock().expect("store fixture lock").clone()
    }
}

fn startup_request() -> super::ModelSwitchRequest {
    super::ModelSwitchRequest::persistent(ModelRef("ollama/qwen3".to_owned()), ThinkingLevel::Off)
}

#[test]
fn startup_publication_preserves_error_and_uncertain_outcomes_atomically() {
    let error_store =
        RecordingStore::with_outcome(Err(UserConfigError::new("fixture write failed")));
    let error = persist_startup_selection(&error_store, &startup_request())
        .expect_err("a failed mutation must block startup");
    assert!(error.to_string().contains("fixture write failed"));
    assert_eq!(error_store.mutations.lock().unwrap().len(), 1);

    let unchanged = RecordingStore::with_outcome(Ok(UserConfigPublication::NotPublished));
    assert_eq!(
        persist_startup_selection(&unchanged, &startup_request()).expect("unchanged publication"),
        UserConfigPublication::NotPublished
    );

    let uncertain =
        RecordingStore::with_outcome(Ok(UserConfigPublication::PublishedDurabilityUncertain {
            diagnostic: "directory sync failed".to_owned(),
        }));
    assert_eq!(
        persist_startup_selection(&uncertain, &startup_request()).expect("uncertain publication"),
        UserConfigPublication::PublishedDurabilityUncertain {
            diagnostic: "directory sync failed".to_owned(),
        }
    );
}

struct FixtureHttp {
    calls: Arc<AtomicUsize>,
}

impl FixtureHttp {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Http for FixtureHttp {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(FixtureCall {
            calls: Arc::clone(&self.calls),
        })
    }
}

struct FixtureCall {
    calls: Arc<AtomicUsize>,
}

impl HttpCall for FixtureCall {
    fn execute(
        &mut self,
        _call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async {
            Ok(OneHopResponse::response(
                200,
                Vec::new(),
                br#"{"models":[]}"#.to_vec(),
            ))
        })
    }
}

#[tokio::test]
async fn ollama_ambient_discovery_stays_loopback_but_explicit_host_is_attempted() {
    let row = ProviderRow::ollama(Url::parse("http://192.0.2.10:11434").expect("fixture endpoint"));
    let fixture = Arc::new(FixtureHttp::new());
    let http: Arc<dyn Http> = fixture.clone();

    let refusal = discover_ollama(&row, &http, false)
        .await
        .expect_err("ambient non-loopback discovery must refuse");
    assert!(refusal.contains("ambient Ollama discovery refuses"));
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 0);

    let report = discover_ollama(&row, &http, true)
        .await
        .expect("an explicit endpoint is admitted to the guarded HTTP port");
    assert!(report.is_complete());
    assert!(report.models().is_empty());
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
}
