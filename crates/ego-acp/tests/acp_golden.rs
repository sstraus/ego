//! `SessionUpdate` projection: golden fixtures for `ego-acp::project`
//! against ACP protocol v1 and v2.
//!
//! See `ARCHITECTURE.md` §7, `plans/m0a-contracts.md` Step 9 and
//! `crates/ego-acp/src/project.rs`'s module doc comment for the full
//! reasoning behind every mapping. Fixtures live in `tests/golden/` as
//! committed data files, matching `ego-core/tests/public_golden.rs`'s
//! pattern, so a projection change shows up as a reviewable data-file diff.
//!
//! ## Fixture provenance
//!
//! This reuses the same 23-event, two-turn log as
//! `ego-core/tests/public_golden.rs` rather than inventing a second one.
//! `ego-core`'s test fixture is private to that crate's own test binary —
//! integration tests are separate compilation units and cannot import each
//! other's non-`pub` test helpers — so this reconstructs it instead of
//! importing it. The payoff is direct comparability: the public-projection
//! golden and these two ACP goldens describe the same input, so a reader
//! can line up all three files and see exactly what each projection does
//! with the same event, rather than reconciling two different logs by hand.
//!
//! "The same" means the same turn and item ids and the same
//! projection-relevant payload sequence — *not* byte-identical events: `ev`
//! calls `EventId::new()`, and the `Forked` event calls `SessionId::new()`,
//! so those two fields differ per construction. Neither is read by either
//! projection.
//!
//! The fixture is a representative session, **not** an exhaustive tour of
//! the domain's 36 event kinds: its 23 events cover 21 distinct kinds
//! (`UserPrompt`, `AssistantDelta`, `AssistantMessage` and `TurnCompleted`
//! each appear more than once) and omit `ReasoningDelta`,
//! `GenerationStarted`, `AttemptSettled`, `UsageCorrected`, both prompt-queue
//! events, and both question events entirely.
//! Exhaustiveness is the job of the per-kind tests below —
//! `reasoning_delta_projects_to_a_thought_chunk_in_both_versions` and
//! `events_with_no_counterpart_in_either_version_project_to_none` cover the
//! omitted kinds — not of this log.
use agent_client_protocol_schema::{v1, v2};
use ego_acp::project::{project_acp_v1, project_acp_v2, project_v1, project_v2};
use ego_core::cost::{
    Amount, CostAttribution, CostDelta, CurrencyCode, Money, PricingRevision, UnknownCost,
};
use ego_core::event::domain::{
    CancelCause, DomainEvent, Envelope, EventKind, ExitReason, GenerationRole, OccupancyProvenance,
    SubagentTerminal,
};
use ego_core::event::hook::{
    HookActivation, HookCatalogueEntryEvidence, HookCatalogueEvidence, HookDecision,
    HookDescriptorEvidence, HookInvocationId, HookOccurrenceKey, HookOccurrenceOwner, HookOutcome,
    HookPlan, HookPlanEntry, HookSourceAuthority, HookSourceLocation,
};
use ego_core::event::id::{
    EventId, GenerationId, ItemId, Producer, QueuedPromptId, Seq, SessionId, SubagentInvocationId,
    TurnId,
};
use ego_core::event::skill::{
    SkillCatalogueEvidence, SkillCatalogueGeneration, SkillContentDigest, SkillDescriptorEvidence,
    SkillId, SkillSourceAuthority, SkillSourceLocation,
};
use ego_core::event::time::Timestamp;
use ego_core::generator::attempt::{AttemptRef, DeliveryCertainty, Outcome, UsageReport};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::{ErrorKind, ReasoningState, StopReason};
use ego_core::permit::{Authority, CanonPath, Escalation, OpName, ToolResult};
use ego_core::public::project_public;
use ego_core::question::{
    QuestionMode, QuestionOption, QuestionOutcome, QuestionRequest, QuestionTarget,
};
use ego_core::run::{RunId, RunSelections};
use ego_core::state::{SessionState, Usage};
use ego_core::store::blob::{BlobId, BlobRef};

// Wrapped in a module named `acp_golden` so `cargo test -p ego-acp
// acp_golden` (the story's VERIFY command) matches these tests by
// substring on the full test path — top-level functions in this file would
// not match, per `ego-core/tests/public_golden.rs`'s own note on the trap.

/// The real chain a connection walks: domain → public → ACP.
///
/// `project_v1`/`project_v2` take a `PublicEvent` now, because `EventSink` is
/// the only live seam a run publishes through and it carries the public
/// projection (`project.rs`'s module doc). Composing here rather than
/// rewriting every fixture keeps these assertions about the ACP mapping while
/// making them run over the events a host would actually be handed.
///
/// An event `project_public` drops never reaches ACP at all — that is the
/// narrowing `project.rs` documents, and it is visible here as a `None`.
fn acp_v1(event: &DomainEvent) -> Option<v1::SessionUpdate> {
    project_public(event).as_ref().and_then(project_v1)
}

fn acp_v2(event: &DomainEvent) -> Option<v2::SessionUpdate> {
    project_public(event).as_ref().and_then(project_v2)
}

/// The same composition for the two entry points that also need reduced state.
fn acp_state_v1(
    event: &DomainEvent,
    state: &SessionState,
    caps: &ModelCapabilities,
) -> Option<v1::SessionUpdate> {
    project_public(event)
        .as_ref()
        .and_then(|public| project_acp_v1(public, state, caps))
}

fn acp_state_v2(
    event: &DomainEvent,
    state: &SessionState,
    caps: &ModelCapabilities,
) -> Option<v2::SessionUpdate> {
    project_public(event)
        .as_ref()
        .and_then(|public| project_acp_v2(public, state, caps))
}

mod acp_golden {
    use super::*;

    fn hook_descriptor() -> HookDescriptorEvidence {
        HookDescriptorEvidence::new(
            HookSourceAuthority::Project,
            HookSourceLocation::parse("hooks/project.json").unwrap(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .parse()
                .unwrap(),
            0,
        )
    }

    fn hook_catalogue_entry() -> HookCatalogueEntryEvidence {
        HookCatalogueEntryEvidence::new(hook_descriptor(), HookActivation::Persistent)
    }

    fn hook_plan_entry() -> HookPlanEntry {
        HookPlanEntry::new(hook_descriptor().id(), HookActivation::Persistent)
    }

    fn blob_ref(bytes: u64) -> BlobRef {
        BlobRef::new(
            BlobId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap(),
            bytes,
        )
    }

    /// Deserialise a fixed UUID literal into a `TurnId`/`ItemId` via their
    /// derived `Deserialize` impl, for deterministic, golden-comparable ids
    /// — same trick as `ego-core/tests/public_golden.rs`'s `id()` helper,
    /// but monomorphised per type rather than generic: `ego-acp` has no
    /// direct `serde` dependency to name `serde::de::DeserializeOwned` with
    /// (only `serde_json`, which is enough once the target type is
    /// concrete).
    fn turn_id(uuid: &str) -> TurnId {
        serde_json::from_str(&format!("{uuid:?}")).unwrap()
    }

    fn item_id(uuid: &str) -> ItemId {
        serde_json::from_str(&format!("{uuid:?}")).unwrap()
    }

    const TURN_1: &str = "00000000-0000-0000-0000-000000000002";
    const TURN_2: &str = "00000000-0000-0000-0000-000000000003";
    const ITEM_USER_1: &str = "00000000-0000-0000-0000-000000000004";
    const ITEM_ASSISTANT_1: &str = "00000000-0000-0000-0000-000000000005";
    const ITEM_TOOL_1: &str = "00000000-0000-0000-0000-000000000006";
    const ITEM_PERM_1: &str = "00000000-0000-0000-0000-000000000007";
    const ITEM_SUB_1: &str = "00000000-0000-0000-0000-000000000008";
    const ITEM_USER_2: &str = "00000000-0000-0000-0000-000000000009";
    const ITEM_ASSISTANT_2: &str = "00000000-0000-0000-0000-00000000000a";

    fn ev(seq: Seq, turn: TurnId, item: Option<ItemId>, kind: EventKind) -> DomainEvent {
        DomainEvent {
            env: Envelope {
                seq,
                id: EventId::new(),
                turn,
                item,
                parent: None,
                ts: Timestamp::from_millis(1_000),
                producer: Producer::Main,
            },
            kind,
        }
    }

    /// Identical to `ego-core/tests/public_golden.rs`'s `fixture_log` — see
    /// this file's module doc comment for why it is reconstructed rather
    /// than shared.
    fn fixture_log() -> Vec<DomainEvent> {
        let turn1: TurnId = turn_id(TURN_1);
        let turn2: TurnId = turn_id(TURN_2);
        let item_user_1: ItemId = item_id(ITEM_USER_1);
        let item_assistant_1: ItemId = item_id(ITEM_ASSISTANT_1);
        let item_tool_1: ItemId = item_id(ITEM_TOOL_1);
        let item_perm_1: ItemId = item_id(ITEM_PERM_1);
        let item_sub_1: ItemId = item_id(ITEM_SUB_1);
        let item_user_2: ItemId = item_id(ITEM_USER_2);
        let item_assistant_2: ItemId = item_id(ITEM_ASSISTANT_2);
        let subagent_invocation = SubagentInvocationId::new();
        let child_run = RunId::new(SessionId::new(), uuid::Uuid::now_v7());

        let mut seq = Seq::default();
        vec![
            ev(
                seq.next(),
                turn1,
                Some(item_user_1),
                EventKind::UserPrompt {
                    text: "list the files".to_string(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_assistant_1),
                EventKind::AssistantDelta {
                    text: "Sure".to_string(),
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_assistant_1),
                EventKind::AssistantDelta {
                    text: ", one sec".to_string(),
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_assistant_1),
                EventKind::AssistantMessage {
                    text: "Sure, one sec".to_string(),
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_tool_1),
                EventKind::ToolCallStarted {
                    name: "bash".to_string(),
                    args: "ls".to_string(),
                    call_id: Some("call_provider_side".to_string()),
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_tool_1),
                EventKind::ToolCallDelta {
                    text: "file1\n".to_string(),
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_tool_1),
                EventKind::ToolCallCompleted {
                    ok: true,
                    result: ToolResult::builtin("file1\nfile2\n"),
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_perm_1),
                EventKind::PermissionRequested {
                    tool: "edit".to_string(),
                    escalation: Escalation::NoRuleCovers {
                        op: OpName::Write,
                        path: CanonPath::new("/etc/hosts"),
                    },
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_perm_1),
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    by: Some(Authority::Boundary),
                    reason: None,
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_sub_1),
                EventKind::SubagentSpawned {
                    invocation: subagent_invocation,
                    child: child_run,
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_sub_1),
                EventKind::SubagentSettled {
                    invocation: subagent_invocation,
                    child: Some(child_run),
                    terminal: SubagentTerminal::Returned {
                        final_message: blob_ref(4),
                    },
                },
            ),
            ev(
                seq.next(),
                turn1,
                None,
                EventKind::UsageUpdated {
                    usage: tokens(120, 45),
                    cost_delta: priced("0.001035"),
                    attempt: None,
                },
            ),
            ev(
                seq.next(),
                turn1,
                None,
                EventKind::ContextWindowUpdated {
                    used_tokens: 165,
                    provenance: OccupancyProvenance::GenerationBoundary,
                    window: None,
                },
            ),
            ev(
                seq.next(),
                turn1,
                None,
                EventKind::Compacted {
                    summary: "earlier turns summarised".to_string(),
                },
            ),
            ev(
                seq.next(),
                turn1,
                None,
                EventKind::Forked {
                    source: SessionId::new(),
                    at_seq: Seq::default(),
                },
            ),
            ev(
                seq.next(),
                turn1,
                None,
                EventKind::HookCatalogueAdmitted {
                    descriptors: HookCatalogueEvidence::new(vec![hook_catalogue_entry()]).unwrap(),
                },
            ),
            ev(
                seq.next(),
                turn1,
                None,
                EventKind::HookLifecycleReached {
                    occurrence: HookOccurrenceKey::new(
                        ego_core::event::hook::HookLifecycle::PreToolUse,
                        HookOccurrenceOwner::Session {
                            session: SessionId::new(),
                        },
                    ),
                    subject: ego_core::event::hook::HookSubject::Session,
                    plan: HookPlan::new(vec![hook_plan_entry()]).unwrap(),
                },
            ),
            ev(
                seq.next(),
                turn1,
                None,
                EventKind::Error {
                    message: "rate limited, retrying".to_string(),
                    kind: ErrorKind::Unclassified,
                },
            ),
            ev(
                seq.next(),
                turn1,
                None,
                EventKind::TurnCompleted {
                    ok: true,
                    exit_reason: ExitReason::Completed,
                },
            ),
            ev(
                seq.next(),
                turn2,
                Some(item_user_2),
                EventKind::UserPrompt {
                    text: "thanks".to_string(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
            ),
            ev(
                seq.next(),
                turn2,
                Some(item_assistant_2),
                EventKind::AssistantMessage {
                    text: "You're welcome".to_string(),
                },
            ),
            ev(
                seq.next(),
                turn2,
                None,
                EventKind::TurnCompleted {
                    ok: true,
                    exit_reason: ExitReason::Completed,
                },
            ),
        ]
    }

    /// Every event kind in the domain's 36-variant projection fixture, once each.
    ///
    /// This is the *source* the `None`-set test selects from by name, so
    /// that a kind renamed or removed upstream breaks a test instead of
    /// silently shrinking the set being checked. It does not by itself prove
    /// every kind is covered — the test below filters it to the kinds it
    /// names, and the rest are covered by the dedicated tests further down.
    ///
    /// **It catches removal and not addition**. The explicit length assertion
    /// below keeps this fixture tied to the current enum; a newly added
    /// variant still requires one deliberately constructed value here.
    fn one_of_each_kind() -> Vec<EventKind> {
        let skill_digest = SkillContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let skill_location = SkillSourceLocation::parse("skills/fixture/SKILL.md").unwrap();
        let skill = SkillDescriptorEvidence {
            id: SkillId::derive(
                SkillSourceAuthority::Project,
                &skill_location,
                "fixture",
                &skill_digest,
            ),
            name: "fixture".to_owned(),
            description: "fixture skill".to_owned(),
            trigger: None,
            argument_hint: None,
            source: SkillSourceAuthority::Project,
            location: skill_location.clone(),
            source_digest: skill_digest,
            user_invocable: true,
            model_invocable: true,
        };
        let skill_catalogue = SkillCatalogueEvidence::new(vec![skill.clone()]).unwrap();
        let skill_generation = SkillCatalogueGeneration::derive(&skill_catalogue);
        let skill_invocation = ego_core::event::skill::SkillInvocationId::new();
        vec![
            EventKind::PromptQueued {
                id: QueuedPromptId::new(),
                text: String::new(),
                images: Vec::new(),
            },
            EventKind::PromptDeliveryExpedited {
                through: QueuedPromptId::new(),
            },
            EventKind::UserPrompt {
                text: String::new(),
                images: Vec::new(),
                queued_prompt: None,
            },
            EventKind::AssistantDelta {
                text: String::new(),
            },
            EventKind::AssistantMessage {
                text: String::new(),
            },
            EventKind::ReasoningDelta {
                text: String::new(),
            },
            EventKind::ReasoningState {
                state: ReasoningState {
                    blob: String::new(),
                    dialect: String::new(),
                },
            },
            EventKind::ToolCallStarted {
                name: String::new(),
                args: String::new(),
                call_id: None,
            },
            EventKind::ToolCallDelta {
                text: String::new(),
            },
            EventKind::ToolCallCompleted {
                ok: true,
                result: ToolResult::builtin(String::new()),
            },
            EventKind::QuestionRequested {
                request: QuestionRequest::new(
                    "Choose",
                    QuestionMode::Single,
                    vec![QuestionOption::new("yes", "Yes")],
                    false,
                ),
                target: QuestionTarget::ModelTool {
                    tool_call: ItemId::new(),
                },
            },
            EventKind::QuestionSettled {
                outcome: QuestionOutcome::Unavailable,
            },
            EventKind::PermissionRequested {
                tool: String::new(),
                escalation: Escalation::NoRuleCovers {
                    op: OpName::Read,
                    path: CanonPath::new(""),
                },
            },
            EventKind::PermissionAmended {
                tool: String::new(),
            },
            EventKind::PermissionDecided {
                allowed: Some(true),
                by: None,
                reason: None,
            },
            EventKind::GenerationStarted {
                attempt: AttemptRef::first(GenerationId::new()),
                role: GenerationRole::Main,
            },
            EventKind::ExecutionObserved {
                observation: ego_core::permit::ExecObservation::new(
                    ego_core::permit::BackendIdentity::new("test", "v1"),
                    ego_core::permit::ExecCaps::empty(),
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                        .try_into()
                        .unwrap(),
                ),
            },
            EventKind::AttemptSettled {
                attempt: AttemptRef::first(GenerationId::new()),
                outcome: Outcome::new(
                    StopReason::Stop,
                    DeliveryCertainty::DefinitelyUnsent,
                    UsageReport::Missing,
                ),
            },
            EventKind::UsageUpdated {
                usage: tokens(1, 1),
                cost_delta: priced("0.000018"),
                attempt: None,
            },
            EventKind::UsageCorrected {
                attempt: AttemptRef::first(GenerationId::new()),
                usage: tokens(1, 1),
                cost_delta: priced("0.000018"),
            },
            EventKind::ContextWindowUpdated {
                used_tokens: 2,
                provenance: OccupancyProvenance::GenerationBoundary,
                window: None,
            },
            EventKind::TurnCompleted {
                ok: true,
                exit_reason: ExitReason::Completed,
            },
            EventKind::CompactionSeeded {
                source: SessionId::new(),
                source_seq: Seq::default(),
                prompt_revision: 0,
                checkpoint: String::new(),
            },
            EventKind::Compacted {
                summary: String::new(),
            },
            EventKind::Forked {
                source: SessionId::new(),
                at_seq: Seq::default(),
            },
            EventKind::HookCatalogueAdmitted {
                descriptors: HookCatalogueEvidence::new(vec![hook_catalogue_entry()]).unwrap(),
            },
            EventKind::HookLifecycleReached {
                occurrence: HookOccurrenceKey::new(
                    ego_core::event::hook::HookLifecycle::PreToolUse,
                    HookOccurrenceOwner::Session {
                        session: SessionId::new(),
                    },
                ),
                subject: ego_core::event::hook::HookSubject::Session,
                plan: HookPlan::new(vec![hook_plan_entry()]).unwrap(),
            },
            EventKind::HookInvocationStarted {
                occurrence: EventId::new(),
                hook: HookDescriptorEvidence::new(
                    HookSourceAuthority::Project,
                    HookSourceLocation::parse("hooks/project.json").unwrap(),
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                        .parse()
                        .unwrap(),
                    0,
                )
                .id(),
                invocation: HookInvocationId::new(),
                lifecycle: ego_core::event::hook::HookLifecycle::PreToolUse,
                subject: ego_core::event::hook::HookSubject::Session,
                activation: ego_core::event::hook::HookActivation::Persistent,
            },
            EventKind::HookInvocationSettled {
                occurrence: EventId::new(),
                hook: HookDescriptorEvidence::new(
                    HookSourceAuthority::Project,
                    HookSourceLocation::parse("hooks/project.json").unwrap(),
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                        .parse()
                        .unwrap(),
                    0,
                )
                .id(),
                invocation: HookInvocationId::new(),
                result: ego_core::event::hook::HookResult::Outcome {
                    outcome: HookOutcome::new(HookDecision::Continue, None),
                },
            },
            EventKind::SubagentRequested {
                invocation: SubagentInvocationId::new(),
                task: blob_ref(0),
                selections: RunSelections::empty(),
            },
            EventKind::SubagentSpawned {
                invocation: SubagentInvocationId::new(),
                child: RunId::new(SessionId::new(), uuid::Uuid::now_v7()),
            },
            EventKind::SubagentSettled {
                invocation: SubagentInvocationId::new(),
                child: Some(RunId::new(SessionId::new(), uuid::Uuid::now_v7())),
                terminal: SubagentTerminal::Returned {
                    final_message: blob_ref(0),
                },
            },
            EventKind::SkillCatalogueAdmitted {
                generation: skill_generation,
                descriptors: skill_catalogue,
            },
            EventKind::SkillInstructionAdmitted {
                invocation: skill_invocation,
                skill: skill.id,
                catalogue_generation: skill_generation,
                source: skill.source,
                location: skill.location.clone(),
                source_digest: skill.source_digest,
                admission: blob_ref(1),
                rendered: blob_ref(2),
                model: None,
                effort: None,
            },
            EventKind::SkillResourceAdmitted {
                invocation: skill_invocation,
                skill: skill.id,
                location: SkillSourceLocation::parse("skills/fixture/reference.md").unwrap(),
                content: blob_ref(3),
            },
            EventKind::Error {
                message: String::new(),
                kind: ErrorKind::Unclassified,
            },
        ]
    }

    fn with_item(kind: EventKind) -> DomainEvent {
        ev(Seq::default(), TurnId::new(), Some(ItemId::new()), kind)
    }

    fn tokens(uncached_input: u64, output: u64) -> Usage {
        Usage {
            uncached_input_tokens: uncached_input,
            output_tokens: output,
            ..Usage::IDENTITY
        }
    }

    fn usd(decimal: &str) -> Money {
        Money::new(
            decimal.parse::<Amount>().expect("a valid test amount"),
            CurrencyCode::new("USD").expect("USD is a valid code"),
        )
    }

    fn priced(decimal: &str) -> CostDelta {
        CostDelta::Known {
            amount: usd(decimal),
            attribution: CostAttribution::Table {
                revision: PricingRevision("test-2026-08".to_string()),
            },
            fx: None,
        }
    }

    /// Capabilities with a stated context window, so `UsageUpdate.size` is
    /// known. `max_context_tokens: None` is the case that must suppress the
    /// update entirely.
    fn caps(max_context_tokens: Option<u64>) -> ModelCapabilities {
        ModelCapabilities {
            schema_version: ModelCapabilities::SCHEMA_VERSION,
            max_context_tokens,
            max_output_tokens: Some(64_000),
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

    /// The reduction of `events`, which is what the state-aware projection
    /// must be handed: the state *after* the event it is projecting.
    fn state_after(events: &[DomainEvent]) -> SessionState {
        let mut state = SessionState::default();
        for ev in events {
            state.apply(ev);
        }
        state
    }

    /// The core v1 projection golden: `project_v1` over the whole fixture
    /// log, `None`s filtered out, one compact JSON object per line — must
    /// match the committed `.jsonl` fixture exactly, up to trailing
    /// whitespace at end of file (`trim_end`, so the fixture can keep the
    /// final newline every editor adds).
    ///
    /// Note what a golden of a `filter_map` cannot show: an event that
    /// projects to `None` leaves no trace here, so this file proves what
    /// *is* emitted and is silent about what was dropped. The named tests
    /// below carry that half — in particular
    /// `v1_loses_a_final_message_that_never_streamed`.
    #[test]
    fn projects_fixture_log_to_committed_v1_jsonl() {
        let expected = include_str!("golden/session_v1.jsonl");
        let actual: String = fixture_log()
            .iter()
            .filter_map(acp_v1)
            .map(|update| serde_json::to_string(&update).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(actual, expected.trim_end());
    }

    /// The core v2 projection golden: same fixture, `project_v2`.
    #[test]
    fn projects_fixture_log_to_committed_v2_jsonl() {
        let expected = include_str!("golden/session_v2.jsonl");
        let actual: String = fixture_log()
            .iter()
            .filter_map(acp_v2)
            .map(|update| serde_json::to_string(&update).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(actual, expected.trim_end());
    }

    /// Explicit, kind-named proof that the *stateless* projections cannot
    /// produce ACP's `UsageUpdate` — not just that the golden happens to
    /// omit it. `UsageUpdate.used` is context occupancy, `size` is the
    /// model's window and `cost` is a cumulative session total; none of the
    /// three is in the event, so a function that sees only the event has
    /// nothing to build one from. The state-aware entry points do produce
    /// it — see `project_acp_emits_usage_update_when_used_and_size_are_known`.
    #[test]
    fn usage_updated_never_projects_through_the_stateless_functions() {
        for kind in [
            EventKind::UsageUpdated {
                usage: tokens(120, 45),
                cost_delta: priced("0.001035"),
                attempt: None,
            },
            EventKind::ContextWindowUpdated {
                used_tokens: 165,
                provenance: OccupancyProvenance::GenerationBoundary,
                window: None,
            },
        ] {
            // With an item attached: the most favourable case for a wrong
            // mapping, since every other arm that needs an id has one here.
            let event = with_item(kind);
            assert_eq!(acp_v1(&event), None);
            assert_eq!(acp_v2(&event), None);
        }
    }

    /// `Forked` projects to `None` in both versions: `session/fork` is a
    /// request/response RPC method, not a `session/update` notification, so
    /// there is no `SessionUpdate` variant to project to regardless of
    /// `unstable_session_fork` being enabled.
    #[test]
    fn forked_never_projects_to_a_session_update() {
        let event = with_item(EventKind::Forked {
            source: SessionId::new(),
            at_seq: Seq::default(),
        });
        assert_eq!(acp_v1(&event), None);
        assert_eq!(acp_v2(&event), None);
    }

    /// Every domain event kind that has no `SessionUpdate` counterpart in
    /// *either* protocol version, matching `project.rs`'s module doc
    /// comment's "no counterpart in either version" list.
    ///
    /// The last three are the accounting kinds, and they reach neither
    /// protocol for a reason one link earlier than the rest: `project_public`
    /// drops them, so nothing about them is public at all. Asserting it here
    /// is what says an attempt's identity and a provider's revision of what
    /// it spent stay inside this process — the `PublicEvent` a host sees
    /// carries the totals, never the ledger they were folded from.
    #[test]
    fn events_with_no_counterpart_in_either_version_project_to_none() {
        assert_eq!(
            one_of_each_kind().len(),
            36,
            "the current internal event set is exhaustive"
        );
        let none_in_both = [
            "prompt_queued",
            "prompt_delivery_expedited",
            "permission_requested",
            "permission_amended",
            "permission_decided",
            "question_requested",
            "question_settled",
            "usage_updated",
            "compacted",
            "forked",
            "hook_catalogue_admitted",
            "hook_lifecycle_reached",
            "hook_invocation_started",
            "hook_invocation_settled",
            "skill_catalogue_admitted",
            "skill_instruction_admitted",
            "skill_resource_admitted",
            "subagent_spawned",
            "subagent_requested",
            "error",
            "generation_started",
            "attempt_settled",
            "usage_corrected",
            "execution_observed",
            // The blob is provider-private and opaque, and ACP describes
            // what the agent did rather than what it must replay.
            "reasoning_state",
        ];
        let kinds: Vec<EventKind> = one_of_each_kind()
            .into_iter()
            .filter(|k| none_in_both.contains(&k.name()))
            .collect();
        assert_eq!(
            kinds.len(),
            none_in_both.len(),
            "every kind named above must exist in the domain's variant set"
        );
        for kind in kinds {
            let event = with_item(kind);
            assert_eq!(
                acp_v1(&event),
                None,
                "{:?} must not project to a v1 SessionUpdate",
                event.kind
            );
            assert_eq!(
                acp_v2(&event),
                None,
                "{:?} must not project to a v2 SessionUpdate",
                event.kind
            );
        }
    }

    /// `AssistantMessage` is the clearest instance of the v1/v2 asymmetry:
    /// v1 has no *stateless* replace-semantics message primitive, v2 does.
    ///
    /// Note the precise claim: not that v1 could never carry this text —
    /// an unstreamed message could legitimately be sent as a v1 chunk — but
    /// that no single fixed v1 output is correct for both histories. See
    /// `v1_loses_a_final_message_that_never_streamed` for the cost.
    #[test]
    fn assistant_message_has_no_stateless_v1_mapping_but_replaces_content_in_v2() {
        let event = with_item(EventKind::AssistantMessage {
            text: "final text".to_string(),
        });
        assert_eq!(acp_v1(&event), None);
        assert!(matches!(
            acp_v2(&event),
            Some(v2::SessionUpdate::AgentMessage(_))
        ));
    }

    /// The accepted M0a limitation, pinned deliberately rather than left as
    /// an unremarked absence in the v1 golden: an assistant message that
    /// never streamed is **lost entirely** from the v1 transcript, while v2
    /// carries it.
    ///
    /// This is turn 2 of `fixture_log` — the user says "thanks" and the
    /// assistant replies "You're welcome" with no preceding
    /// `AssistantDelta`. A v1 client sees the prompt and then silence.
    ///
    /// If a future change makes v1 emit something here, this test fails and
    /// forces a decision rather than letting the behaviour drift: see
    /// `project.rs`'s "Known limitation" section for why neither branch is
    /// universally correct and why the fix belongs to the M0b transport
    /// adapter, which alone knows what a given client already rendered.
    #[test]
    fn v1_loses_a_final_message_that_never_streamed() {
        let item = item_id(ITEM_ASSISTANT_2);
        let event = ev(
            Seq::default(),
            turn_id(TURN_2),
            Some(item),
            EventKind::AssistantMessage {
                text: "You're welcome".to_string(),
            },
        );

        // The loss, stated outright.
        assert_eq!(
            acp_v1(&event),
            None,
            "v1 currently drops a non-streamed final message; if this now \
             emits an update, re-read project.rs's Known limitation section"
        );

        // And the same event under v2, to show the text is not
        // intrinsically unrepresentable — only unrepresentable statelessly
        // in v1.
        assert_eq!(
            acp_v2(&event),
            Some(v2::SessionUpdate::AgentMessage(
                v2::AgentMessage::new(ITEM_ASSISTANT_2).content(vec!["You're welcome".into()])
            ))
        );

        // The v1 golden must not accidentally start containing it either:
        // the whole fixture log emits nothing carrying this text in v1.
        let v1_lines: Vec<String> = fixture_log()
            .iter()
            .filter_map(acp_v1)
            .map(|u| serde_json::to_string(&u).unwrap())
            .collect();
        assert!(
            !v1_lines.iter().any(|line| line.contains("You're welcome")),
            "v1 golden unexpectedly carries the non-streamed message: {v1_lines:#?}"
        );
    }

    /// `ToolCallDelta` is the tool-call-shaped instance of the same
    /// asymmetry: v1 has no append primitive for tool-call content (its
    /// `ToolCallUpdate.content` replaces the whole collection, so emitting
    /// one increment needs the accumulated content this stateless function
    /// does not have), while v2 has a dedicated append-only
    /// `ToolCallContentChunk`.
    #[test]
    fn tool_call_delta_has_no_stateless_v1_mapping_but_appends_in_v2() {
        let event = with_item(EventKind::ToolCallDelta {
            text: "partial output".to_string(),
        });
        assert_eq!(acp_v1(&event), None);
        assert!(matches!(
            acp_v2(&event),
            Some(v2::SessionUpdate::ToolCallContentChunk(_))
        ));
    }

    /// `ReasoningDelta` is the one domain kind `fixture_log` omits, so
    /// without this test neither version's reasoning mapping has any
    /// runtime coverage at all — both goldens would be equally green with
    /// the arm deleted.
    ///
    /// Unlike the message pair above there is no asymmetry here: reasoning
    /// is append-only in both versions (the domain has no final-reasoning
    /// counterpart to `AssistantMessage`), so both map to a thought chunk.
    #[test]
    fn reasoning_delta_projects_to_a_thought_chunk_in_both_versions() {
        let item = ItemId::new();
        let event = ev(
            Seq::default(),
            TurnId::new(),
            Some(item),
            EventKind::ReasoningDelta {
                text: "weighing the options".to_string(),
            },
        );
        let id = serde_json::to_value(item).unwrap();
        let id = id.as_str().unwrap();

        assert_eq!(
            acp_v1(&event),
            Some(v1::SessionUpdate::AgentThoughtChunk(
                v1::ContentChunk::new("weighing the options".into())
                    .message_id(Some(v1::MessageId::new(id)))
            ))
        );
        assert_eq!(
            acp_v2(&event),
            Some(v2::SessionUpdate::AgentThoughtChunk(v2::ContentChunk::new(
                "weighing the options".into(),
                id
            )))
        );
    }

    /// `TurnCompleted` has no v1 `SessionUpdate` counterpart (turn
    /// completion rides the `session/prompt` RPC response's `stop_reason`
    /// there) but maps to v2's `StateUpdate` — added specifically to report
    /// foreground-work state independently of the RPC response. **Every**
    /// `ExitReason` value is exercised here — including all three
    /// `CancelCause`s, which must project alike — since the two-turn fixture
    /// only ever uses `Completed`. The list is exhaustive on purpose: a new
    /// variant that nobody maps would otherwise reach a host as whatever the
    /// projection's catch-all happened to be.
    ///
    /// Each `exit_reason` is run with **both** `ok` values, which is what
    /// actually proves `project.rs`'s claim that `ok` "carries no further
    /// information beyond what `stop_reason` already conveys": if the
    /// projection ever started reading `ok`, one of the two runs would
    /// diverge. Pairing every case with `ok: true` — including
    /// `Error`/`Budget`, which are not successes — would instead have
    /// quietly asserted that only the combinations nobody disputes work.
    #[test]
    fn turn_completed_maps_to_v2_idle_state_by_exit_reason_alone() {
        for (exit_reason, expected_stop_reason) in [
            (ExitReason::Completed, v2::StopReason::EndTurn),
            // All three causes flatten to one ACP stop reason: the protocol
            // has no field for who cancelled, and the cause survives in the
            // durable log and the exit code instead.
            (
                ExitReason::Cancelled(CancelCause::User),
                v2::StopReason::Cancelled,
            ),
            (
                ExitReason::Cancelled(CancelCause::Timeout),
                v2::StopReason::Cancelled,
            ),
            (
                ExitReason::Cancelled(CancelCause::Host),
                v2::StopReason::Cancelled,
            ),
            (
                ExitReason::Provider,
                v2::StopReason::Other("_provider".to_string()),
            ),
            (
                ExitReason::Internal,
                v2::StopReason::Other("_internal".to_string()),
            ),
            (
                ExitReason::PermissionRequired,
                v2::StopReason::Other("_permission".to_string()),
            ),
            (
                ExitReason::Budget,
                v2::StopReason::Other("_budget".to_string()),
            ),
        ] {
            for ok in [true, false] {
                let event = ev(
                    Seq::default(),
                    TurnId::new(),
                    None,
                    EventKind::TurnCompleted { ok, exit_reason },
                );
                assert_eq!(acp_v1(&event), None, "{exit_reason:?} ok={ok}");
                assert_eq!(
                    acp_v2(&event),
                    Some(v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                        v2::IdleStateUpdate::new().stop_reason(expected_stop_reason.clone())
                    ))),
                    "{exit_reason:?} ok={ok}"
                );
            }
        }
    }

    /// **This test used to assert the opposite, and the seam change is why.**
    ///
    /// v1's `ContentChunk.message_id` is optional, so the old
    /// `DomainEvent`-based mapping emitted a chunk for an item-less event —
    /// content without continuity, on the grounds that it beat dropping the
    /// text. That mapping no longer sees the event: `project_public` maps an
    /// item-less event to `None`, so it never reaches ACP at all.
    ///
    /// Kept and inverted rather than deleted, because the fact it pins is the
    /// one that replaced it, and it is a *loss* somebody could otherwise
    /// reintroduce by accident: a future `project_public` that started
    /// tolerating a missing item id would silently change what a host sees,
    /// and nothing else in this file would notice.
    ///
    /// Table-driven over the same three arms for the same reason as before —
    /// they are written separately in `project.rs`, so one regressing alone
    /// would go unnoticed by a single-variant test.
    #[test]
    fn an_item_less_event_reaches_neither_protocol_because_the_public_stream_drops_it() {
        let kinds = [
            EventKind::UserPrompt {
                text: "orphaned".to_string(),
                images: Vec::new(),
                queued_prompt: None,
            },
            EventKind::AssistantDelta {
                text: "orphaned".to_string(),
            },
            EventKind::ReasoningDelta {
                text: "orphaned".to_string(),
            },
        ];
        for kind in kinds {
            let name = kind.name();
            let event = ev(Seq::default(), TurnId::new(), None, kind);
            assert_eq!(
                project_public(&event),
                None,
                "{name} reached the public stream without an item id"
            );
            assert_eq!(acp_v1(&event), None, "v1 {name}");
            assert_eq!(acp_v2(&event), None, "v2 {name}");
        }
    }

    /// v2 requires a `messageId`/`toolCallId` on every message- and
    /// tool-call-shaped update, so **every** content-bearing event without
    /// an item id projects to `None` there — unlike v1, where the three
    /// chunk-shaped ones still project.
    ///
    /// All seven are checked, not just the two the earlier version of this
    /// file covered: this `None` is real content loss if an item id is ever
    /// absent in production, so the blast radius should be visible in one
    /// place.
    #[test]
    fn every_v2_content_bearing_event_without_item_id_projects_to_none() {
        let kinds = [
            EventKind::UserPrompt {
                text: "orphaned".to_string(),
                images: Vec::new(),
                queued_prompt: None,
            },
            EventKind::AssistantDelta {
                text: "orphaned".to_string(),
            },
            EventKind::AssistantMessage {
                text: "orphaned".to_string(),
            },
            EventKind::ReasoningDelta {
                text: "orphaned".to_string(),
            },
            EventKind::ToolCallStarted {
                name: "bash".to_string(),
                args: "ls".to_string(),
                call_id: Some("call_provider_side".to_string()),
            },
            EventKind::ToolCallDelta {
                text: "orphaned".to_string(),
            },
            EventKind::ToolCallCompleted {
                ok: true,
                result: ToolResult::builtin("orphaned"),
            },
        ];
        for kind in kinds {
            let name = kind.name();
            let event = ev(Seq::default(), TurnId::new(), None, kind);
            assert_eq!(acp_v2(&event), None, "{name}");
        }
    }

    /// Tool-call ids are required in **both** versions, so every tool-call
    /// event without an item id projects to `None` in both — unlike the
    /// chunk-shaped message events, which v1 still projects.
    #[test]
    fn every_tool_call_event_without_item_id_projects_to_none_in_both_versions() {
        let kinds = [
            EventKind::ToolCallStarted {
                name: "bash".to_string(),
                args: "ls".to_string(),
                call_id: Some("call_provider_side".to_string()),
            },
            EventKind::ToolCallDelta {
                text: "orphaned".to_string(),
            },
            EventKind::ToolCallCompleted {
                ok: true,
                result: ToolResult::builtin("orphaned"),
            },
        ];
        for kind in kinds {
            let name = kind.name();
            let event = ev(Seq::default(), TurnId::new(), None, kind);
            assert_eq!(acp_v1(&event), None, "v1 {name}");
            assert_eq!(acp_v2(&event), None, "v2 {name}");
        }
    }

    // --- the state-aware entry points ------------------------------------

    /// A three-event lead-in: one priced generation, then the occupancy it
    /// implies. Returned whole so a test can project the last event with
    /// the state that includes it.
    fn usage_log() -> Vec<DomainEvent> {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        vec![
            ev(
                seq.next(),
                turn,
                None,
                EventKind::UsageUpdated {
                    usage: tokens(120, 45),
                    cost_delta: priced("0.001035"),
                    attempt: None,
                },
            ),
            ev(
                seq.next(),
                turn,
                None,
                EventKind::ContextWindowUpdated {
                    used_tokens: 165,
                    provenance: OccupancyProvenance::GenerationBoundary,
                    window: None,
                },
            ),
        ]
    }

    /// The happy path: occupancy known, window known, cost known in one
    /// currency — so `UsageUpdate` carries all three fields, identically in
    /// both protocol versions.
    #[test]
    fn project_acp_emits_usage_update_when_used_and_size_are_known() {
        let log = usage_log();
        let state = state_after(&log);
        let caps = caps(Some(200_000));
        let last = log.last().unwrap();

        let v1_update = acp_state_v1(last, &state, &caps).expect("used and size are both known");
        let v1::SessionUpdate::UsageUpdate(v1_usage) = v1_update else {
            panic!("expected a v1 UsageUpdate, got {v1_update:?}");
        };
        assert_eq!(v1_usage.used, 165);
        assert_eq!(v1_usage.size, 200_000);
        let cost = v1_usage.cost.expect("the cumulative cost is known");
        assert_eq!(cost.currency, "USD");
        // 0.001035 is not exactly representable in binary floating point,
        // which is the entire reason the domain keeps it as an exact
        // decimal and converts only here.
        assert!((cost.amount - 0.001035).abs() < 1e-12);

        let v2_update = acp_state_v2(last, &state, &caps).expect("used and size are both known");
        let v2::SessionUpdate::UsageUpdate(v2_usage) = v2_update else {
            panic!("expected a v2 UsageUpdate, got {v2_update:?}");
        };
        assert_eq!((v2_usage.used, v2_usage.size), (165, 200_000));
    }

    /// The window reported is the one the generation that produced the
    /// occupancy actually ran under, not the one preflight resolved.
    ///
    /// A turn can end up on a fallback model: `turn/mod.rs` walks
    /// `config.fallbacks` and resolves a fresh profile per candidate, while
    /// the sink captured its `ModelCapabilities` once, before the loop
    /// (`ego-cli/src/run.rs`, `sink_factory.create`). So a run whose primary
    /// model failed and whose fallback answered would divide the fallback's
    /// occupancy by the primary's window — and a host would render a session
    /// as comfortably inside a limit it is actually over. The occupancy event
    /// carries the denominator with the numerator for that reason: they are
    /// one observation, and the caps are only what to say when no generation
    /// has reported yet.
    #[test]
    fn project_acp_reports_the_window_the_generation_ran_under_not_the_one_preflight_resolved() {
        let mut log = usage_log();
        let EventKind::ContextWindowUpdated { window, .. } = &mut log[1].kind else {
            panic!("the fixture's second event is the occupancy report");
        };
        *window = Some(32_768);
        let state = state_after(&log);
        let last = log.last().unwrap();

        let update =
            acp_state_v1(last, &state, &caps(Some(200_000))).expect("used and size are both known");
        let v1::SessionUpdate::UsageUpdate(usage) = update else {
            panic!("expected a v1 UsageUpdate");
        };
        assert_eq!(
            usage.size, 32_768,
            "the update divided by the model preflight resolved, not the one that generated"
        );
        assert_eq!(usage.used, 165);
    }

    /// A generation that stated no window leaves the caps in charge.
    ///
    /// The complement of the test above, and the reason the state's window is
    /// an `Option` rather than a number with a zero: a model whose facts carry
    /// no `max_context_tokens` reports occupancy all the same, and reading its
    /// silence as a window would divide by nothing.
    #[test]
    fn an_occupancy_report_that_names_no_window_falls_back_to_the_resolved_capabilities() {
        let log = usage_log();
        let state = state_after(&log);
        assert_eq!(state.context.window, None, "the fixture states no window");

        let update = acp_state_v1(log.last().unwrap(), &state, &caps(Some(200_000)))
            .expect("used and size are both known");
        let v1::SessionUpdate::UsageUpdate(usage) = update else {
            panic!("expected a v1 UsageUpdate");
        };
        assert_eq!(usage.size, 200_000);
    }

    /// The RFD is explicit that an agent must not send `UsageUpdate` when
    /// the context size is unknown, and clients must tolerate its absence.
    /// A model with no stated context window is exactly that case.
    #[test]
    fn project_acp_emits_nothing_when_the_context_window_size_is_unknown() {
        let log = usage_log();
        let state = state_after(&log);
        let caps = caps(None);
        let last = log.last().unwrap();
        assert_eq!(acp_state_v1(last, &state, &caps), None);
        assert_eq!(acp_state_v2(last, &state, &caps), None);
    }

    /// Occupancy has to be *reported*; it cannot be inferred from token
    /// totals. A session that has recorded usage but no
    /// `ContextWindowUpdated` therefore has no `used` to send — and the
    /// wrong answer here would be to sum the token counts, which is the
    /// specific mistake `ego-acp-usage-verdict` ruled out.
    #[test]
    fn project_acp_emits_nothing_when_occupancy_was_never_reported() {
        let log = vec![usage_log().remove(0)];
        let state = state_after(&log);
        assert_eq!(state.context.used_tokens, None);
        assert_ne!(state.usage.total_tokens(), 0, "tokens were recorded");

        let caps = caps(Some(200_000));
        assert_eq!(acp_state_v1(&log[0], &state, &caps), None);
        assert_eq!(acp_state_v2(&log[0], &state, &caps), None);
    }

    /// `cost` is optional in the ACP schema, and an unknown cumulative cost
    /// must simply be omitted — never sent as zero. Occupancy and window are
    /// still known here, so the update itself is emitted: an unpriceable
    /// generation must not cost the client its context readout.
    #[test]
    fn project_acp_omits_cost_when_the_cumulative_total_is_unknown() {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let log = vec![
            ev(
                seq.next(),
                turn,
                None,
                EventKind::UsageUpdated {
                    usage: tokens(120, 45),
                    cost_delta: CostDelta::Unknown(UnknownCost::NoPricingQuote),
                    attempt: None,
                },
            ),
            ev(
                seq.next(),
                turn,
                None,
                EventKind::ContextWindowUpdated {
                    used_tokens: 165,
                    provenance: OccupancyProvenance::GenerationBoundary,
                    window: None,
                },
            ),
        ];
        let state = state_after(&log);
        let caps = caps(Some(200_000));

        let update = acp_state_v1(log.last().unwrap(), &state, &caps)
            .expect("used and size are known even when cost is not");
        let v1::SessionUpdate::UsageUpdate(usage) = update else {
            panic!("expected a UsageUpdate");
        };
        assert_eq!((usage.used, usage.size), (165, 200_000));
        assert_eq!(usage.cost, None, "an unknown cost must be absent, not zero");
    }

    /// The state-aware functions are supersets, not replacements: for every
    /// event that does not need state they must agree with the stateless
    /// ones exactly. Otherwise a caller would have to keep calling both,
    /// and the two paths could drift.
    #[test]
    fn project_acp_agrees_with_the_stateless_projection_on_every_other_kind() {
        let log = fixture_log();
        let state = state_after(&log);
        let caps = caps(Some(200_000));
        for event in &log {
            if matches!(
                event.kind,
                EventKind::UsageUpdated { .. } | EventKind::ContextWindowUpdated { .. }
            ) {
                continue;
            }
            let name = event.kind.name();
            assert_eq!(
                acp_state_v1(event, &state, &caps),
                acp_v1(event),
                "v1 {name}"
            );
            assert_eq!(
                acp_state_v2(event, &state, &caps),
                acp_v2(event),
                "v2 {name}"
            );
        }
    }
}
