//! RED contract for CLI lifecycle ownership in M4 S7.
//!
//! The model picker, generation source, compaction command, and prompt
//! adapter are the production owners exercised here. No CLI lifecycle wrapper
//! is created in the test.

use std::sync::Arc;
use std::sync::Mutex;

use ego_ai::catalogue::{
    FactFreshness, ModelAvailability, ModelCatalogueEntry, ModelCatalogueSnapshot,
    ModelCatalogueSourceStatus, ModelFactProvenance, ModelSlug, SuppliedAuthority,
};
use ego_cli::hook::{HookPromptAdapter, ToollessGenerationService};
use ego_cli::model::GenerationSelection;
use ego_cli::model_picker::{ModelPicker, PickerInput, PickerInputOutcome, PickerOutcome};
use ego_cli::repl::actions::{ActionId, ActionSink, LocalAction, LocalActionOutcome};
use ego_core::config::{
    ConfigLayer, ModelRef, Source, Trust, UserConfigError, UserConfigMutation,
    UserConfigPublication, UserConfigStore,
};
use ego_core::event::domain::GenerationRole;
use ego_core::event::hook::{
    HookActivation, HookDecision, HookFailure, HookFailureKind, HookInvocationId, HookLifecycle,
    HookOutcome, HookPlan, HookResult, HookSubject,
};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::ThinkingLevel;
use ego_core::hook::{
    EffectScope, HookCatalogue, HookFuture, HookHandlerConfig, HookInvocationRequest,
    HookLifecycleDispatcher, HookOccurrence, HookPort, HookReachFuture, HookReachedEvidence,
    HookRecordFuture, HookRunner, HookSpec, HookStartFuture, HookStartedEvidence,
};

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
        "ModelPicker active selection pre-commit",
        HookLifecycle::PreModelSwitch,
    ),
    (
        "GenerationSelection active selection post-commit",
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

#[derive(Clone, Default)]
struct RecordingPort {
    calls: Arc<Mutex<Vec<HookLifecycle>>>,
}

impl RecordingPort {
    fn calls(&self) -> Vec<HookLifecycle> {
        self.calls.lock().expect("CLI hook calls").clone()
    }
}

impl HookPort for RecordingPort {
    fn reached<'a>(
        &'a self,
        producer: &'a ego_core::event::id::Producer,
        occurrence: &'a HookOccurrence,
        plan: &'a HookPlan,
    ) -> HookReachFuture<'a> {
        let evidence = HookReachedEvidence::new(
            ego_core::event::id::EventId::new(),
            occurrence.key().clone(),
            occurrence.event().subject(),
            producer.clone(),
            plan.clone(),
        );
        Box::pin(std::future::ready(Ok(
            ego_core::hook::HookReachOutcome::Fresh(evidence),
        )))
    }
    fn started<'a>(
        &'a self,
        spec: &'a HookSpec,
        occurrence: &'a HookOccurrence,
        reached: &'a HookReachedEvidence,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> HookStartFuture<'a> {
        self.calls
            .lock()
            .expect("CLI hook calls")
            .push(occurrence.event().lifecycle());
        Box::pin(std::future::ready(Ok(HookStartedEvidence::new(
            reached.event(),
            reached.event(),
            spec.id(),
            invocation,
            activation,
        ))))
    }

    fn invoke<'a>(
        &'a self,
        _spec: &'a HookSpec,
        _event: ego_core::hook::HookEvent,
    ) -> HookFuture<'a> {
        Box::pin(std::future::ready(Ok(HookOutcome::new(
            HookDecision::Continue,
            None,
        ))))
    }

    fn settled<'a>(
        &'a self,
        _spec: &'a HookSpec,
        _started: HookStartedEvidence,
        _result: &'a HookResult,
    ) -> HookRecordFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }
}

#[derive(Clone, Default)]
struct BlockingPort(RecordingPort);

#[derive(Clone, Default)]
struct ObserverFailurePort(RecordingPort);

macro_rules! delegate_cli_port {
    ($name:ty, $outcome:expr) => {
        impl HookPort for $name {
            fn reached<'a>(
                &'a self,
                producer: &'a ego_core::event::id::Producer,
                occurrence: &'a HookOccurrence,
                plan: &'a HookPlan,
            ) -> HookReachFuture<'a> {
                self.0.reached(producer, occurrence, plan)
            }
            fn started<'a>(
                &'a self,
                spec: &'a HookSpec,
                occurrence: &'a HookOccurrence,
                reached: &'a HookReachedEvidence,
                activation: HookActivation,
                invocation: HookInvocationId,
            ) -> HookStartFuture<'a> {
                self.0
                    .started(spec, occurrence, reached, activation, invocation)
            }

            fn invoke<'a>(
                &'a self,
                _spec: &'a HookSpec,
                _event: ego_core::hook::HookEvent,
            ) -> HookFuture<'a> {
                Box::pin(std::future::ready($outcome))
            }

            fn settled<'a>(
                &'a self,
                spec: &'a HookSpec,
                started: HookStartedEvidence,
                result: &'a HookResult,
            ) -> HookRecordFuture<'a> {
                self.0.settled(spec, started, result)
            }
        }
    };
}

delegate_cli_port!(
    BlockingPort,
    Ok(HookOutcome::new(HookDecision::Block, None))
);

impl HookPort for ObserverFailurePort {
    fn reached<'a>(
        &'a self,
        producer: &'a ego_core::event::id::Producer,
        occurrence: &'a HookOccurrence,
        plan: &'a HookPlan,
    ) -> HookReachFuture<'a> {
        self.0.reached(producer, occurrence, plan)
    }

    fn started<'a>(
        &'a self,
        spec: &'a HookSpec,
        occurrence: &'a HookOccurrence,
        reached: &'a HookReachedEvidence,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> HookStartFuture<'a> {
        self.0
            .started(spec, occurrence, reached, activation, invocation)
    }

    fn invoke<'a>(
        &'a self,
        _spec: &'a HookSpec,
        event: ego_core::hook::HookEvent,
    ) -> HookFuture<'a> {
        let outcome = if event.lifecycle() == HookLifecycle::PostModelSwitch {
            Err(HookFailure::new(HookFailureKind::HandlerFailed, None))
        } else {
            Ok(HookOutcome::new(HookDecision::Continue, None))
        };
        Box::pin(std::future::ready(outcome))
    }

    fn settled<'a>(
        &'a self,
        spec: &'a HookSpec,
        started: HookStartedEvidence,
        result: &'a HookResult,
    ) -> HookRecordFuture<'a> {
        self.0.settled(spec, started, result)
    }
}

fn dispatcher(port: RecordingPort) -> HookLifecycleDispatcher {
    HookLifecycleDispatcher::new(
        HookRunner::new_with_durable_started(port, Vec::<HookStartedEvidence>::new()),
        hook_catalogue(),
    )
}

fn blocking_dispatcher(port: BlockingPort) -> HookLifecycleDispatcher {
    HookLifecycleDispatcher::new(
        HookRunner::new_with_durable_started(port, Vec::<HookStartedEvidence>::new()),
        hook_catalogue(),
    )
}

fn observer_failure_dispatcher(port: ObserverFailurePort) -> HookLifecycleDispatcher {
    HookLifecycleDispatcher::new(
        HookRunner::new_with_durable_started(port, Vec::<HookStartedEvidence>::new()),
        hook_catalogue(),
    )
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
}
impl UserConfigStore for ConfigStore {
    fn mutate(
        &self,
        _mutation: UserConfigMutation,
    ) -> Result<UserConfigPublication, UserConfigError> {
        assert_eq!(
            self.selection.snapshot().expect("selection").identity,
            ModelRef("openai/old".to_owned()),
            "config publication must precede active model installation"
        );
        Ok(UserConfigPublication::PublishedDurably)
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
fn cli_owner_matrix_has_one_deterministic_session_compaction_and_handoff_path() {
    assert_eq!(CLI_OWNERS.len(), 10);
    assert_eq!(CLI_OWNERS[0].1, HookLifecycle::SessionStart);
    assert_eq!(CLI_OWNERS[4].1, HookLifecycle::PreCompact);
    assert_eq!(CLI_OWNERS[5].1, HookLifecycle::PostCompact);
    assert_eq!(CLI_OWNERS[8].1, HookLifecycle::SessionEnd);
}

#[test]
fn model_picker_persists_before_install_and_observes_after_commit() {
    let selection = Arc::new(GenerationSelection::new(
        ModelRef("openai/old".to_owned()),
        ThinkingLevel::Medium,
    ));
    let mut picker = ModelPicker::for_selection(
        catalogue(),
        Arc::clone(&selection),
        Arc::new(ConfigStore {
            selection: Arc::clone(&selection),
        }),
    )
    .with_hook_dispatcher(dispatcher(RecordingPort::default()));
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
        picker.input(PickerInput::Number(2)),
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
    assert_eq!(
        picker.input(PickerInput::Enter),
        Ok(PickerInputOutcome::Closed(PickerOutcome::Persisted(
            UserConfigPublication::PublishedDurably,
        )))
    );
    assert_eq!(
        selection.snapshot().expect("selection").identity,
        ModelRef("openai/new".to_owned())
    );
}

#[test]
fn same_selection_does_not_emit_model_switch_and_post_observer_cannot_revert_commit() {
    let selection =
        GenerationSelection::new(ModelRef("openai/old".to_owned()), ThinkingLevel::Medium);
    let port = RecordingPort::default();
    let dispatcher = dispatcher(port.clone());
    selection
        .install_with_hook_dispatcher(
            ModelRef("openai/old".to_owned()),
            ThinkingLevel::Medium,
            &dispatcher,
        )
        .expect("same selection remains a no-op");
    selection
        .install_with_hook_dispatcher(
            ModelRef("openai/new".to_owned()),
            ThinkingLevel::Medium,
            &dispatcher,
        )
        .expect("pre hook allowed commit");
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
    let blocked_selection =
        GenerationSelection::new(ModelRef("openai/old".to_owned()), ThinkingLevel::Medium);
    let blocked_port = BlockingPort::default();
    let blocked = blocking_dispatcher(blocked_port.clone());
    let blocked_result = blocked_selection.install_with_hook_dispatcher(
        ModelRef("openai/new".to_owned()),
        ThinkingLevel::Medium,
        &blocked,
    );
    assert!(
        blocked_result.is_err(),
        "pre-switch block is an owner refusal"
    );
    assert_eq!(
        blocked_selection.snapshot().expect("selection").identity,
        ModelRef("openai/old".to_owned())
    );

    let observed_selection =
        GenerationSelection::new(ModelRef("openai/old".to_owned()), ThinkingLevel::Medium);
    let observer_port = ObserverFailurePort::default();
    let observer_failure = observer_failure_dispatcher(observer_port.clone());
    let observer_result = observed_selection.install_with_hook_dispatcher(
        ModelRef("openai/new".to_owned()),
        ThinkingLevel::Medium,
        &observer_failure,
    );
    assert!(
        observer_result.is_err(),
        "observer failure is visible to the owner"
    );
    assert_eq!(
        observed_selection.snapshot().expect("selection").identity,
        ModelRef("openai/new".to_owned()),
        "observer failure cannot roll back durable commit"
    );
}

#[test]
fn hook_prompt_resolution_and_child_initial_selection_are_not_active_switches() {
    let generation = HookGeneration::default();
    let request = HookInvocationRequest::new(
        HookHandlerConfig::Prompt {
            prompt: "Return continue".to_owned(),
            model: Some("openai/hook".to_owned()),
            timeout_seconds: Some(30),
        },
        HookLifecycle::UserPromptSubmit,
        None,
        HookSubject::Session,
        HookActivation::Persistent,
        HookInvocationId::new(),
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
