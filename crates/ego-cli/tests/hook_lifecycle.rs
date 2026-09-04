//! RED contract for CLI lifecycle ownership in M4 S7.
//!
//! The model picker, generation source, compaction command, and prompt
//! adapter are the production owners exercised here. No CLI lifecycle wrapper
//! is created in the test.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use ego_ai::catalogue::{
    FactFreshness, ModelAvailability, ModelCatalogueEntry, ModelCatalogueSnapshot,
    ModelCatalogueSourceStatus, ModelFactProvenance, ModelSlug, SuppliedAuthority,
};
use ego_cli::hook::{HookPromptAdapter, ToollessGenerationService};
use ego_cli::model::GenerationSelection;
use ego_cli::model_picker::{ModelPicker, PickerInput, PickerInputOutcome, PickerOutcome};
use ego_cli::provider_http::McpProcessScope;
use ego_cli::repl::actions::{ActionId, ActionSink, LocalAction, LocalActionOutcome};
use ego_cli::run::RunModelSwitchOwner;
use ego_core::config::{
    ConfigLayer, ModelRef, RuleSnapshot, Source, Trust, UserConfigError, UserConfigMutation,
    UserConfigMutationFacts, UserConfigMutationResult, UserConfigPublication, UserConfigSnapshot,
    UserConfigStore, UserConfigTarget,
};
use ego_core::event::domain::GenerationRole;
use ego_core::event::hook::{
    HookActivation, HookContentDigest, HookDecision, HookFailure, HookFailureKind, HookId,
    HookInvocationId, HookLifecycle, HookOccurrenceKey, HookOccurrenceOwner, HookOutcome, HookPlan,
    HookResult, HookSourceAuthority, HookSourceLocation,
};
use ego_core::event::id::{EventId, SessionId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::ThinkingLevel;
use ego_core::hook::{
    EffectScope, HookCatalogue, HookCausality, HookEvent, HookFuture, HookHandlerConfig,
    HookInvocationPort, HookInvocationRequest, HookJournalPort, HookLifecycleDispatcher,
    HookOccurrence, HookReachError, HookReachOutcome, HookReachedEvidence, HookRunError,
    HookRunner, HookSpec, HookStartError, HookStartedEvidence, SessionHookRuntime,
    SessionHookState,
};
use ego_core::sink::NullSink;
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::turn::{CancelToken, Journal, SharedJournal};
use ego_mcp::{McpRegistryController, McpRegistryRuntime};

const CLI_OWNERS: [(&str, HookLifecycle); 10] = [
    (
        "interactive session activation",
        HookLifecycle::SessionStart,
    ),
    ("instruction admission", HookLifecycle::InstructionsLoaded),
    ("TurnLoop prompt ingress", HookLifecycle::UserPromptSubmit),
    ("typed prompt expansion", HookLifecycle::UserPromptExpansion),
    (
        "CliCompactionCommand pre-transition",
        HookLifecycle::PreCompact,
    ),
    (
        "CliCompactionCommand post-publication",
        HookLifecycle::PostCompact,
    ),
    (
        "run owner active selection pre-commit",
        HookLifecycle::PreModelSwitch,
    ),
    (
        "run owner active selection post-commit",
        HookLifecycle::PostModelSwitch,
    ),
    ("interactive session end", HookLifecycle::SessionEnd),
    ("run handoff stop observation", HookLifecycle::Stop),
];

fn hook_catalogue() -> HookCatalogue {
    let source = CLI_OWNERS
        .iter()
        .map(|(_, lifecycle)| format!("[[hooks]]\nevent = \"{}\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"continue\"\n", lifecycle.as_str()))
        .collect::<String>();
    HookCatalogue::admit_typed(
        &[ConfigLayer::parse(Source::User, &source).expect("admitted CLI hooks")],
        &[],
        Trust::Trusted,
    )
    .expect("CLI hook catalogue")
}

fn fixture_hook_id(ordinal: u32) -> HookId {
    HookId::derive(
        HookSourceAuthority::Project,
        &HookSourceLocation::parse("hooks/cli.toml").expect("hook source"),
        &HookContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("hook digest"),
        ordinal,
    )
}

#[derive(Clone, Default)]
struct RecordingPort {
    calls: Arc<Mutex<Vec<HookLifecycle>>>,
}

impl RecordingPort {
    fn calls(&self) -> Vec<HookLifecycle> {
        self.calls.lock().expect("CLI hook calls").clone()
    }
}

impl HookJournalPort for RecordingPort {
    fn reached(
        &self,
        producer: &ego_core::event::id::Producer,
        occurrence: &HookOccurrence,
        plan: &HookPlan,
    ) -> Result<HookReachOutcome, HookReachError> {
        let evidence = HookReachedEvidence::new(
            ego_core::event::id::EventId::new(),
            occurrence.key().clone(),
            occurrence.event().subject(),
            producer.clone(),
            plan.clone(),
        );
        Ok(HookReachOutcome::Fresh(evidence))
    }
    fn started(
        &self,
        spec: &HookSpec,
        occurrence: &HookOccurrence,
        reached: &HookReachedEvidence,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> Result<HookStartedEvidence, HookStartError> {
        self.calls
            .lock()
            .expect("CLI hook calls")
            .push(occurrence.event().lifecycle());
        Ok(HookStartedEvidence::new(
            reached.event(),
            reached.event(),
            spec.id(),
            invocation,
            activation,
        ))
    }

    fn settled(
        &self,
        _occurrence: &HookOccurrence,
        _started: HookStartedEvidence,
        _result: &HookResult,
    ) -> Result<EventId, HookRunError> {
        Ok(EventId::new())
    }
}

impl HookInvocationPort for RecordingPort {
    fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
        self.calls
            .lock()
            .expect("CLI hook calls")
            .push(request.lifecycle());
        Box::pin(std::future::ready(Ok(HookOutcome::new(
            HookDecision::Continue,
            None,
        ))))
    }
}

#[derive(Clone, Default)]
struct BlockingPort(RecordingPort);

#[derive(Clone, Default)]
struct ObserverFailurePort(RecordingPort);

macro_rules! delegate_cli_port {
    ($name:ty, $outcome:expr) => {
        impl HookJournalPort for $name {
            fn reached(
                &self,
                producer: &ego_core::event::id::Producer,
                occurrence: &HookOccurrence,
                plan: &HookPlan,
            ) -> Result<HookReachOutcome, HookReachError> {
                self.0.reached(producer, occurrence, plan)
            }
            fn started(
                &self,
                spec: &HookSpec,
                occurrence: &HookOccurrence,
                reached: &HookReachedEvidence,
                activation: HookActivation,
                invocation: HookInvocationId,
            ) -> Result<HookStartedEvidence, HookStartError> {
                self.0
                    .started(spec, occurrence, reached, activation, invocation)
            }

            fn settled(
                &self,
                occurrence: &HookOccurrence,
                started: HookStartedEvidence,
                result: &HookResult,
            ) -> Result<EventId, HookRunError> {
                self.0.settled(occurrence, started, result)
            }
        }

        impl HookInvocationPort for $name {
            fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
                self.0
                    .calls
                    .lock()
                    .expect("CLI hook calls")
                    .push(request.lifecycle());
                Box::pin(std::future::ready($outcome))
            }
        }
    };
}

delegate_cli_port!(
    BlockingPort,
    Ok(HookOutcome::new(HookDecision::Block, None))
);

impl HookJournalPort for ObserverFailurePort {
    fn reached(
        &self,
        producer: &ego_core::event::id::Producer,
        occurrence: &HookOccurrence,
        plan: &HookPlan,
    ) -> Result<HookReachOutcome, HookReachError> {
        self.0.reached(producer, occurrence, plan)
    }

    fn started(
        &self,
        spec: &HookSpec,
        occurrence: &HookOccurrence,
        reached: &HookReachedEvidence,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> Result<HookStartedEvidence, HookStartError> {
        self.0
            .started(spec, occurrence, reached, activation, invocation)
    }

    fn settled(
        &self,
        occurrence: &HookOccurrence,
        started: HookStartedEvidence,
        result: &HookResult,
    ) -> Result<EventId, HookRunError> {
        self.0.settled(occurrence, started, result)
    }
}

impl HookInvocationPort for ObserverFailurePort {
    fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
        self.0
            .calls
            .lock()
            .expect("CLI hook calls")
            .push(request.lifecycle());
        let outcome = if request.lifecycle() == HookLifecycle::PostModelSwitch {
            Err(HookFailure::new(HookFailureKind::HandlerFailed, None))
        } else {
            Ok(HookOutcome::new(HookDecision::Continue, None))
        };
        Box::pin(std::future::ready(outcome))
    }
}

#[derive(Default)]
struct RuntimeStore;

impl SessionStore for RuntimeStore {
    fn append(
        &self,
        _id: &SessionId,
        _event: &ego_core::event::domain::DomainEvent,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    fn read(
        &self,
        _id: &SessionId,
    ) -> Result<futures::stream::BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError>
    {
        Ok(Box::pin(futures::stream::empty()))
    }

    fn compare_and_append_run(
        &self,
        _session: &SessionId,
        _expected_revision: Option<ego_core::run::RunRevision>,
        _transaction: ego_core::run::RunTransaction,
    ) -> Result<ego_core::run::RunAppend, StoreError> {
        Err(StoreError::NotFound)
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(Vec::new())
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(1_000)
    }
}

fn runtime<P>(port: P) -> SessionHookRuntime<'static>
where
    P: HookInvocationPort + 'static,
{
    let session = SessionId::new();
    let store: &'static RuntimeStore = Box::leak(Box::new(RuntimeStore));
    let clock: &'static FixedClock = Box::leak(Box::new(FixedClock));
    let sink: &'static NullSink = Box::leak(Box::new(NullSink));
    let journal = SharedJournal::new(Journal::new(
        session,
        store,
        clock,
        sink,
        ego_core::event::id::Producer::Main,
        ego_core::state::SessionState::default(),
    ));
    SessionHookState::new(
        HookLifecycleDispatcher::new(HookRunner::new(), hook_catalogue()),
        journal,
        CancelToken::new(),
        std::time::Instant::now() + Duration::from_secs(30),
    )
    .bind(Arc::new(port))
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        max_context_tokens: Some(128_000),
        max_output_tokens: Some(16_000),
        max_tools: Some(128),
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::Portable,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
    }
}

fn catalogue() -> Arc<ModelCatalogueSnapshot> {
    let provenance = ModelFactProvenance::supplied(SuppliedAuthority::System);
    let rows = ["openai/old", "openai/new"]
        .into_iter()
        .map(|slug| ModelCatalogueEntry {
            slug: ModelSlug::parse(slug).expect("current model slug"),
            capabilities: capabilities(),
            metering: ego_core::public::MeteringKind::Currency,
            provenance: provenance.clone(),
            freshness: FactFreshness::Current,
            availability: ModelAvailability::Available,
        })
        .collect();
    Arc::new(ModelCatalogueSnapshot::from_entries_and_sources(
        rows,
        vec![ModelCatalogueSourceStatus {
            provider: "openai".to_owned(),
            provenance,
            freshness: FactFreshness::Current,
            availability: ModelAvailability::Available,
        }],
    ))
}

#[derive(Clone)]
struct ConfigStore {
    selection: Arc<GenerationSelection>,
    publications: Arc<Mutex<usize>>,
}
impl UserConfigStore for ConfigStore {
    fn snapshot(&self, target: UserConfigTarget) -> Result<UserConfigSnapshot, UserConfigError> {
        Ok(UserConfigSnapshot {
            values: ego_core::config::ManagedConfigKey::for_target(target.clone())
                .iter()
                .copied()
                .map(|key| (key, None))
                .collect(),
            target,
        })
    }

    fn rule_snapshot(&self, target: UserConfigTarget) -> Result<RuleSnapshot, UserConfigError> {
        Ok(RuleSnapshot {
            target,
            permissions_present: false,
            allow: Vec::new(),
            deny: Vec::new(),
        })
    }

    fn mutate(
        &self,
        _mutation: UserConfigMutation,
    ) -> Result<UserConfigMutationResult, UserConfigError> {
        assert_eq!(
            self.selection.snapshot().expect("selection").identity,
            ModelRef("openai/old".to_owned()),
            "config publication must precede active model installation"
        );
        *self.publications.lock().expect("config publications") += 1;
        Ok(UserConfigMutationResult {
            target: UserConfigTarget::Base,
            facts: UserConfigMutationFacts::Set {
                key: ego_core::config::ManagedConfigKey::Model,
                value: ego_core::config::ManagedConfigValue::parse(
                    ego_core::config::ManagedConfigKey::Model,
                    "'openai/new'",
                )
                .unwrap(),
            },
            changed: true,
            publication: UserConfigPublication::PublishedDurably,
        })
    }
}

impl ConfigStore {
    fn publications(&self) -> usize {
        *self.publications.lock().expect("config publications")
    }
}

#[derive(Clone, Default)]
struct HookGeneration {
    roles: Arc<Mutex<Vec<GenerationRole>>>,
}
impl ToollessGenerationService for HookGeneration {
    fn generate(
        &self,
        request: ego_cli::hook::ToollessGenerationRequest,
    ) -> ego_cli::hook::GenerationFuture<'_> {
        self.roles
            .lock()
            .expect("generation roles")
            .push(request.role());
        assert_eq!(request.role(), GenerationRole::Hook);
        Box::pin(std::future::ready(Ok(ego_cli::hook::GeneratedHookResult {
            raw: br#"{"version":1,"decision":"continue"}"#.to_vec(),
            evidence: ego_core::store::evidence::EvidenceRef::new(
                ego_core::store::evidence::EvidenceId::parse(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .expect("evidence id"),
                37,
            ),
            delivery: ego_core::generator::attempt::DeliveryCertainty::DefinitelySent,
        })))
    }
}

#[test]
fn cli_owner_matrix_routes_each_declared_lifecycle_once_in_order() {
    assert_eq!(CLI_OWNERS.len(), 10);
    assert_eq!(CLI_OWNERS[0].1, HookLifecycle::SessionStart);
    assert_eq!(CLI_OWNERS[4].1, HookLifecycle::PreCompact);
    assert_eq!(CLI_OWNERS[5].1, HookLifecycle::PostCompact);
    assert_eq!(CLI_OWNERS[8].1, HookLifecycle::SessionEnd);
    let names = CLI_OWNERS
        .iter()
        .map(|(owner, _)| *owner)
        .collect::<HashSet<_>>();
    let lifecycles = CLI_OWNERS
        .iter()
        .map(|(_, lifecycle)| *lifecycle)
        .collect::<HashSet<_>>();
    assert_eq!(names.len(), CLI_OWNERS.len());
    assert_eq!(lifecycles.len(), CLI_OWNERS.len());
    assert_eq!(
        CLI_OWNERS.map(|(_, lifecycle)| lifecycle),
        [
            HookLifecycle::SessionStart,
            HookLifecycle::InstructionsLoaded,
            HookLifecycle::UserPromptSubmit,
            HookLifecycle::UserPromptExpansion,
            HookLifecycle::PreCompact,
            HookLifecycle::PostCompact,
            HookLifecycle::PreModelSwitch,
            HookLifecycle::PostModelSwitch,
            HookLifecycle::SessionEnd,
            HookLifecycle::Stop,
        ]
    );

    let session = SessionId::new();
    let journal_port = RecordingPort::default();
    let invocation_port = RecordingPort::default();
    let dispatcher = HookLifecycleDispatcher::new(HookRunner::new(), hook_catalogue());
    for (_, lifecycle) in CLI_OWNERS {
        let occurrence = HookOccurrence::new(
            HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Session { session }),
            HookEvent::session(lifecycle),
            HookCausality::session(session, TurnId::new()),
        )
        .expect("CLI lifecycle occurrence");
        futures::executor::block_on(dispatcher.dispatch(
            &journal_port,
            &invocation_port,
            ego_core::turn::CancelToken::new(),
            std::time::Instant::now() + std::time::Duration::from_secs(30),
            ego_core::event::id::Producer::Main,
            occurrence,
        ))
        .expect("CLI lifecycle route");
    }
    assert_eq!(
        invocation_port.calls(),
        CLI_OWNERS.map(|(_, lifecycle)| lifecycle),
        "one shared dispatcher routes the complete CLI owner matrix"
    );
}

#[test]
fn model_picker_persists_before_install_and_observes_after_commit() {
    let selection = Arc::new(GenerationSelection::new(
        ModelRef("openai/old".to_owned()),
        ThinkingLevel::Medium,
    ));
    let config = Arc::new(ConfigStore {
        selection: Arc::clone(&selection),
        publications: Arc::new(Mutex::new(0)),
    });
    let mut picker =
        ModelPicker::for_selection(catalogue(), Arc::clone(&selection), config.clone());
    assert_eq!(
        ActionSink::invoke(
            &mut picker,
            LocalAction {
                id: ActionId::Model,
                busy: false
            },
        ),
        LocalActionOutcome::Completed
    );
    assert_eq!(
        picker.input(PickerInput::Number(1)),
        Ok(PickerInputOutcome::Consumed)
    );
    assert!(matches!(
        picker.input(PickerInput::Enter),
        Ok(PickerInputOutcome::FocusChanged(_))
    ));
    assert_eq!(
        picker.input(PickerInput::Number(1)),
        Ok(PickerInputOutcome::Consumed)
    );
    let PickerInputOutcome::Closed(PickerOutcome::ModelSwitch(request)) =
        picker.input(PickerInput::Enter).expect("staged switch")
    else {
        panic!("picker must stage a typed model-switch request");
    };
    assert_eq!(request.target(), ModelRef("openai/new".to_owned()));
    assert_eq!(request.thinking(), ThinkingLevel::Off);
    assert!(request.is_persistent());
    assert_eq!(
        selection.snapshot().expect("selection").identity,
        ModelRef("openai/old".to_owned()),
        "picker stages a request without mutating the active session"
    );
    assert_eq!(config.publications(), 0, "run owner owns publication");
}

#[test]
fn same_selection_does_not_emit_model_switch_and_post_observer_cannot_revert_commit() {
    let selection = Arc::new(GenerationSelection::new(
        ModelRef("openai/old".to_owned()),
        ThinkingLevel::Medium,
    ));
    let config = Arc::new(ConfigStore {
        selection: Arc::clone(&selection),
        publications: Arc::new(Mutex::new(0)),
    });
    let mut picker =
        ModelPicker::for_selection(catalogue(), Arc::clone(&selection), config.clone());
    ActionSink::invoke(
        &mut picker,
        LocalAction {
            id: ActionId::Model,
            busy: false,
        },
    );
    picker.input(PickerInput::Number(2)).expect("old model");
    picker.input(PickerInput::Enter).expect("model focus");
    picker.input(PickerInput::Number(4)).expect("medium effort");
    let PickerInputOutcome::Closed(PickerOutcome::ModelSwitch(request)) =
        picker.input(PickerInput::Enter).expect("staged no-op")
    else {
        panic!("picker must stage a typed model-switch request");
    };
    assert_eq!(request.target(), ModelRef("openai/old".to_owned()));
    assert_eq!(request.thinking(), ThinkingLevel::Medium);
    assert!(request.is_persistent());
    let port = RecordingPort::default();
    let owner = RunModelSwitchOwner::new(
        Arc::clone(&selection),
        config.clone(),
        runtime(port.clone()),
        TurnId::new(),
        None,
    );
    futures::executor::block_on(owner.apply(request)).expect("same selection remains a no-op");
    assert_eq!(
        selection.snapshot().expect("selection").identity,
        ModelRef("openai/old".to_owned())
    );
    assert_eq!(config.publications(), 0, "same selection does not publish");
    assert!(port.calls().is_empty(), "same selection emits no hooks");
}

#[test]
fn model_switch_owner_persists_and_installs_only_after_pre_gate() {
    let selection = Arc::new(GenerationSelection::new(
        ModelRef("openai/old".to_owned()),
        ThinkingLevel::Medium,
    ));
    let config = Arc::new(ConfigStore {
        selection: Arc::clone(&selection),
        publications: Arc::new(Mutex::new(0)),
    });
    let mut picker =
        ModelPicker::for_selection(catalogue(), Arc::clone(&selection), config.clone());
    ActionSink::invoke(
        &mut picker,
        LocalAction {
            id: ActionId::Model,
            busy: false,
        },
    );
    picker.input(PickerInput::Number(1)).expect("new model");
    picker.input(PickerInput::Enter).expect("model focus");
    picker.input(PickerInput::Number(1)).expect("off effort");
    let PickerInputOutcome::Closed(PickerOutcome::ModelSwitch(request)) =
        picker.input(PickerInput::Enter).expect("staged switch")
    else {
        panic!("picker must stage a typed model-switch request");
    };
    assert_eq!(request.target(), ModelRef("openai/new".to_owned()));
    assert_eq!(request.thinking(), ThinkingLevel::Off);
    assert!(request.is_persistent());
    let port = RecordingPort::default();
    let owner = RunModelSwitchOwner::new(
        Arc::clone(&selection),
        config.clone(),
        runtime(port.clone()),
        TurnId::new(),
        None,
    );
    futures::executor::block_on(owner.apply(request)).expect("pre gate allows switch");
    assert_eq!(config.publications(), 1);
    assert_eq!(
        selection.snapshot().expect("selection").identity,
        ModelRef("openai/new".to_owned())
    );
    assert_eq!(
        port.calls(),
        vec![
            HookLifecycle::PreModelSwitch,
            HookLifecycle::PostModelSwitch
        ]
    );
}

#[test]
fn model_switch_block_preserves_old_selection_and_post_failure_does_not_revert_commit() {
    let blocked_selection = Arc::new(GenerationSelection::new(
        ModelRef("openai/old".to_owned()),
        ThinkingLevel::Medium,
    ));
    let blocked_config = Arc::new(ConfigStore {
        selection: Arc::clone(&blocked_selection),
        publications: Arc::new(Mutex::new(0)),
    });
    let blocked_port = BlockingPort::default();
    let blocked = runtime(blocked_port.clone());
    let blocked_request = staged_request(Arc::clone(&blocked_selection), blocked_config.clone());
    let blocked_owner = RunModelSwitchOwner::new(
        Arc::clone(&blocked_selection),
        blocked_config.clone(),
        blocked,
        TurnId::new(),
        None,
    );
    let blocked_result = futures::executor::block_on(blocked_owner.apply(blocked_request));
    assert!(
        blocked_result.is_err(),
        "pre-switch block is an owner refusal"
    );
    assert_eq!(
        blocked_selection.snapshot().expect("selection").identity,
        ModelRef("openai/old".to_owned())
    );
    assert_eq!(blocked_config.publications(), 0);

    let observed_selection = Arc::new(GenerationSelection::new(
        ModelRef("openai/old".to_owned()),
        ThinkingLevel::Medium,
    ));
    let observed_config = Arc::new(ConfigStore {
        selection: Arc::clone(&observed_selection),
        publications: Arc::new(Mutex::new(0)),
    });
    let observer_port = ObserverFailurePort::default();
    let observer_failure = runtime(observer_port.clone());
    let observer_request = staged_request(Arc::clone(&observed_selection), observed_config.clone());
    let observer_owner = RunModelSwitchOwner::new(
        Arc::clone(&observed_selection),
        observed_config.clone(),
        observer_failure,
        TurnId::new(),
        None,
    );
    let observer_result = futures::executor::block_on(observer_owner.apply(observer_request));
    assert!(
        observer_result.is_err(),
        "observer failure is visible to the owner"
    );
    assert_eq!(
        observed_selection.snapshot().expect("selection").identity,
        ModelRef("openai/new".to_owned()),
        "observer failure cannot roll back durable commit"
    );
    assert_eq!(observed_config.publications(), 1);
}

fn staged_request(
    selection: Arc<GenerationSelection>,
    config: Arc<ConfigStore>,
) -> ego_cli::model_picker::ModelSwitchRequest {
    let mut picker = ModelPicker::for_selection(catalogue(), selection, config);
    ActionSink::invoke(
        &mut picker,
        LocalAction {
            id: ActionId::Model,
            busy: false,
        },
    );
    picker.input(PickerInput::Number(1)).expect("new model");
    picker.input(PickerInput::Enter).expect("model focus");
    picker.input(PickerInput::Number(1)).expect("off effort");
    let PickerInputOutcome::Closed(PickerOutcome::ModelSwitch(request)) =
        picker.input(PickerInput::Enter).expect("staged switch")
    else {
        panic!("picker must stage a typed model-switch request");
    };
    request
}

#[test]
fn hook_prompt_resolution_and_child_initial_selection_are_not_active_switches() {
    let generation = HookGeneration::default();
    let session = SessionId::new();
    let turn = TurnId::new();
    let invocation = HookInvocationId::new();
    let lifecycle = HookLifecycle::UserPromptSubmit;
    let request = HookInvocationRequest::new(
        HookStartedEvidence::new(
            EventId::new(),
            EventId::new(),
            fixture_hook_id(0),
            invocation,
            HookActivation::Persistent,
        ),
        HookCausality::session(session, turn),
        HookHandlerConfig::Prompt {
            prompt: "Return continue".to_owned(),
            model: Some("openai/hook".to_owned()),
            timeout_seconds: Some(30),
        },
        HookEvent::session(lifecycle),
        EffectScope::new(std::time::Duration::from_secs(30)),
    );
    let result =
        futures::executor::block_on(HookPromptAdapter::new(generation.clone()).invoke(request))
            .expect("hook prompt resolves through Hook role");
    assert_eq!(result.decision(), HookDecision::Continue);
    assert_eq!(
        generation
            .roles
            .lock()
            .expect("generation roles")
            .as_slice(),
        &[GenerationRole::Hook]
    );
}

#[test]
fn direct_mcp_scope_keeps_runtime_owner_and_sessionless_controller_access() {
    let scope =
        futures::executor::block_on(McpProcessScope::online(ego_core::turn::CancelToken::new()))
            .expect("MCP process scope opens its owned runtime");
    let runtime: &Arc<McpRegistryRuntime> = scope.runtime();
    let controller: &Arc<McpRegistryController> = scope.controller();
    assert!(
        Arc::ptr_eq(controller, runtime.controller()),
        "the controller comes from the process scope's sole runtime owner"
    );
}
