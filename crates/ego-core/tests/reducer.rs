//! `DomainEvent` and the pure sync reducer: replay determinism, totality
//! and causal-parent resolution.
//!
//! See `ARCHITECTURE.md` §1 and `plans/m0a-contracts.md` Step 2.

use ego_core::config::Mode;
use ego_core::cost::{
    Amount, CostAttribution, CostDelta, CumulativeCost, CurrencyCode, FxQuote, Money,
    PricingRevision,
};
use ego_core::event::domain::{
    DomainEvent, Envelope, EventKind, ExitReason, GenerationRole, OccupancyProvenance,
    SubagentTerminal,
};
use ego_core::event::hook::{
    HookActivation, HookCatalogueEntryEvidence, HookCatalogueEvidence, HookDecision,
    HookDescriptorEvidence, HookInvocationId, HookOccurrenceKey, HookOccurrenceOwner, HookOutcome,
    HookPlan, HookPlanEntry, HookSourceAuthority, HookSourceLocation,
};
use ego_core::event::id::{AttemptId, GenerationId};
use ego_core::event::id::{
    EventId, ItemId, Producer, Seq, SessionId, SubagentInvocationId, TurnId,
};
use ego_core::event::time::Timestamp;
use ego_core::generator::attempt::{AttemptRef, DeliveryCertainty, Outcome, UsageReport};
use ego_core::generator::types::{ErrorKind, StopReason};
use ego_core::permit::{Authority, CanonPath, Escalation, OpName, ToolResult};
use ego_core::run::{RequestId, RunId, RunSelections};
use ego_core::state::{
    PauseState, PermissionEvent, RefusalReason, SessionState, SubagentEvent, Usage,
};
use ego_core::store::blob::{BlobId, BlobRef};

// Wrapped in a module named `reducer` so `cargo test -p ego-core reducer`
// (the story's VERIFY command) matches these tests by substring: cargo's
// test filter matches on the full test path, not the source file name, and
// `tests/reducer.rs`'s bare functions wouldn't otherwise contain "reducer".
mod reducer {
    use super::*;

    #[test]
    fn detached_exec_settlement_replays_as_one_model_notification() {
        use ego_core::event::id::DetachedExecId;
        use ego_core::permit::{DetachedExecOutcome, DetachedExecSnapshot};

        let id = DetachedExecId::new();
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let started = ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::DetachedExecStarted { id },
        );
        let settled = ev(
            seq.next(),
            turn,
            Some(ItemId::new()),
            Some(started.env.id),
            EventKind::DetachedExecSettled {
                snapshot: DetachedExecSnapshot {
                    id,
                    outcome: Some(DetachedExecOutcome::Exited {
                        code: 7,
                        result: ToolResult::builtin("complete output reference"),
                    }),
                    stdout_tail: b"final output".to_vec(),
                    stderr_tail: Vec::new(),
                    stdout_omitted: 100_000,
                    stderr_omitted: 0,
                },
            },
        );
        let state = SessionState::replay([started, settled]);
        assert!(state.unsettled_detached_execs().is_empty());
        let (conversation, repairs) =
            ego_core::generator::history::conversation_from(&state).unwrap();
        assert!(repairs.is_empty());
        let text = format!("{conversation:?}");
        assert!(text.contains(&id.to_string()), "{text}");
        assert!(text.contains("7"), "{text}");
        assert!(text.contains("final output"), "{text}");
        assert_eq!(text.matches("final output").count(), 1, "{text}");
    }

    fn blob_ref(bytes: u64) -> BlobRef {
        BlobRef::new(
            BlobId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap(),
            bytes,
        )
    }

    fn ev(
        seq: Seq,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        kind: EventKind,
    ) -> DomainEvent {
        DomainEvent {
            env: Envelope {
                seq,
                id: EventId::new(),
                turn,
                item,
                parent,
                ts: Timestamp::from_millis(1_000),
                producer: Producer::Main,
            },
            kind,
        }
    }

    /// Tokens in the two categories most events exercise; the cache
    /// categories default to zero and are set explicitly where they matter.
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

    /// A priced cost delta under the default test pricing revision.
    fn priced(decimal: &str) -> CostDelta {
        CostDelta::Known {
            amount: usd(decimal),
            attribution: CostAttribution::Table {
                revision: PricingRevision("test-2026-08".to_string()),
            },
            fx: None,
        }
    }

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

    /// A realistic single-turn log exercising every `EventKind` variant, with
    /// each event's `parent` pointing at the previous event's id.
    #[allow(unused_assignments)] // the final push!'s parent update is never re-read
    fn fixture_log() -> Vec<DomainEvent> {
        let turn = TurnId::new();
        let msg_item = ItemId::new();
        let reasoning_item = ItemId::new();
        let tool_item = ItemId::new();
        let source_session = SessionId::new();
        let subagent_invocation = SubagentInvocationId::new();
        let child_run = RunId::new(SessionId::new(), uuid::Uuid::now_v7());
        let mut seq = Seq::default();
        let mut parent: Option<EventId> = None;
        let mut log = Vec::new();

        macro_rules! push {
            ($item:expr, $kind:expr) => {{
                let e = ev(seq.next(), turn, $item, parent, $kind);
                parent = Some(e.env.id);
                log.push(e);
            }};
        }

        push!(
            Some(msg_item),
            EventKind::UserPrompt {
                text: "list the files".to_string(),
                images: Vec::new(),
                queued_prompt: None,
            }
        );
        push!(
            Some(msg_item),
            EventKind::AssistantDelta {
                text: "Sure".to_string()
            }
        );
        push!(
            Some(msg_item),
            EventKind::AssistantDelta {
                text: ", one sec".to_string()
            }
        );
        push!(
            Some(msg_item),
            EventKind::AssistantMessage {
                text: "Sure, one sec".to_string()
            }
        );
        push!(
            Some(reasoning_item),
            EventKind::ReasoningDelta {
                text: "the user wants a listing".to_string()
            }
        );
        push!(
            Some(tool_item),
            EventKind::ToolCallStarted {
                name: "glob".to_string(),
                args: "{\"pattern\":\"*\"}".to_string(),
                call_id: Some("call_a".to_string()),
            }
        );
        push!(
            Some(tool_item),
            EventKind::ToolCallDelta {
                text: "a.txt\n".to_string()
            }
        );
        push!(
            Some(tool_item),
            EventKind::ToolCallCompleted {
                ok: true,
                result: ToolResult::builtin("a.txt\nb.txt")
            }
        );
        push!(
            Some(tool_item),
            EventKind::PermissionRequested {
                tool: "read".to_string(),
                escalation: Escalation::NoRuleCovers {
                    op: OpName::Read,
                    path: CanonPath::new("/ws"),
                },
            }
        );
        push!(
            Some(tool_item),
            EventKind::PermissionDecided {
                allowed: Some(true),
                by: Some(Authority::Mode { mode: Mode::Edits }),
                reason: None,
            }
        );
        push!(
            None,
            EventKind::UsageUpdated {
                usage: tokens(42, 7),
                cost_delta: priced("0.000441"),
                attempt: None,
            }
        );
        push!(
            None,
            EventKind::ContextWindowUpdated {
                used_tokens: 49,
                provenance: OccupancyProvenance::GenerationBoundary,
                window: None,
            }
        );
        let hook = hook_descriptor();
        push!(
            None,
            EventKind::HookCatalogueAdmitted {
                descriptors: HookCatalogueEvidence::new(vec![HookCatalogueEntryEvidence::new(
                    hook.clone(),
                    HookActivation::Persistent
                ),])
                .unwrap(),
            }
        );
        let invocation = HookInvocationId::new();
        push!(
            None,
            EventKind::HookInvocationStarted {
                occurrence: EventId::new(),
                hook: hook.id(),
                invocation,
                lifecycle: ego_core::event::hook::HookLifecycle::PreToolUse,
                subject: ego_core::event::hook::HookSubject::Session,
                activation: ego_core::event::hook::HookActivation::Persistent,
            }
        );
        push!(
            None,
            EventKind::HookInvocationSettled {
                occurrence: EventId::new(),
                hook: hook.id(),
                invocation,
                result: ego_core::event::hook::HookResult::Outcome {
                    outcome: HookOutcome::new(HookDecision::Continue, None),
                },
            }
        );
        push!(
            None,
            EventKind::SubagentRequested {
                invocation: subagent_invocation,
                task: blob_ref(12),
                selections: RunSelections::empty(),
            }
        );
        push!(
            None,
            EventKind::SubagentSpawned {
                invocation: subagent_invocation,
                child: child_run,
            }
        );
        push!(
            None,
            EventKind::SubagentSettled {
                invocation: subagent_invocation,
                child: Some(child_run),
                terminal: SubagentTerminal::Returned {
                    final_message: blob_ref(4),
                },
            }
        );
        push!(
            None,
            EventKind::Compacted {
                summary: "earlier turns summarised".to_string()
            }
        );
        push!(
            None,
            EventKind::Forked {
                source: source_session,
                at_seq: Seq::default(),
                snapshot: None
            }
        );
        push!(
            None,
            EventKind::Error {
                message: "a recoverable hiccup".to_string(),
                kind: ErrorKind::Unclassified,
            }
        );
        push!(
            None,
            EventKind::TurnCompleted {
                ok: true,
                exit_reason: ExitReason::Completed
            }
        );

        log
    }

    #[test]
    fn replay_is_deterministic() {
        let log = fixture_log();

        let mut a = SessionState::default();
        for e in &log {
            a.apply(e);
        }
        let mut b = SessionState::default();
        for e in &log {
            b.apply(e);
        }

        assert_eq!(a, b);
        // The fixture does real work; pin a few facts so this isn't vacuous.
        assert_eq!(a.usage.uncached_input_tokens, 42);
        assert_eq!(a.usage.output_tokens, 7);
        assert_eq!(
            a.cost,
            CumulativeCost::Known {
                total: usd("0.000441")
            }
        );
        assert_eq!(a.context.used_tokens, Some(49));
        assert_eq!(
            a.context.provenance,
            Some(OccupancyProvenance::GenerationBoundary)
        );
        assert_eq!(a.errors, vec!["a recoverable hiccup".to_string()]);
        assert_eq!(a.unresolved_parents, Vec::<EventId>::new());
        assert_eq!(a.last_seq, log.last().unwrap().env.seq);
    }

    #[test]
    fn causal_parent_chain_resolves() {
        let state = {
            let mut s = SessionState::default();
            for e in &fixture_log() {
                s.apply(e);
            }
            s
        };
        assert!(
            state.unresolved_parents.is_empty(),
            "every event's parent is the previous event in the fixture, so all must resolve"
        );
    }

    #[test]
    fn unknown_causal_parent_is_tracked_not_rejected() {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let ghost_parent = EventId::new(); // never applied to this state
        let e = ev(
            seq.next(),
            turn,
            None,
            Some(ghost_parent),
            EventKind::Error {
                message: "boom".to_string(),
                kind: ErrorKind::Unclassified,
            },
        );
        let id = e.env.id;

        let mut state = SessionState::default();
        state.apply(&e); // must not panic
        assert_eq!(state.unresolved_parents, vec![id]);
        // The event is still applied despite the unresolved parent.
        assert_eq!(state.errors, vec!["boom".to_string()]);
    }

    #[test]
    fn known_events_accumulate_so_later_children_resolve() {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();
        let e1 = ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::UserPrompt {
                text: "hi".to_string(),
                images: Vec::new(),
                queued_prompt: None,
            },
        );
        assert!(state.known_events.is_empty());
        state.apply(&e1);
        assert!(state.known_events.contains(&e1.env.id));

        let e2 = ev(
            seq.next(),
            turn,
            None,
            Some(e1.env.id),
            EventKind::AssistantMessage {
                text: "hello".to_string(),
            },
        );
        state.apply(&e2);
        assert!(state.unresolved_parents.is_empty());
    }

    #[test]
    fn apply_does_not_reject_or_reorder_out_of_order_seq() {
        let turn = TurnId::new();
        let mut counter = Seq::default();
        let seq1 = counter.next();
        let seq2 = counter.next();

        let logically_second = ev(
            seq2,
            turn,
            None,
            None,
            EventKind::Error {
                message: "applied first".to_string(),
                kind: ErrorKind::Unclassified,
            },
        );
        let logically_first = ev(
            seq1,
            turn,
            None,
            None,
            EventKind::Error {
                message: "applied second".to_string(),
                kind: ErrorKind::Unclassified,
            },
        );

        let mut state = SessionState::default();
        state.apply(&logically_second);
        assert_eq!(state.last_seq, seq2);

        // Feeding a smaller seq after a larger one must not panic, and apply
        // must not "fix up" the regression: it just records what it applied.
        state.apply(&logically_first);
        assert_eq!(state.last_seq, seq1);
        assert_eq!(
            state.errors.len(),
            2,
            "both events are applied regardless of seq order"
        );
    }

    /// Table-driven totality proof: every one of the 17 `EventKind`
    /// variants, applied with every degenerate axis stacked at once —
    /// empty strings, `u64::MAX` payloads, no item, an unresolvable parent,
    /// a duplicate event id shared across all 17 events, and a `seq` that
    /// regresses a previously-applied higher one. `apply` must not panic on
    /// any of it. If a variant is missing from the table, this test does
    /// not cover it — the `EventKind::name()` cross-check below keeps the
    /// table honest against the enum.
    #[test]
    fn apply_is_total_for_every_variant_under_maximally_degenerate_conditions() {
        let turn = TurnId::new();
        let mut state = SessionState::default();

        // Prime last_seq above zero so every event below regresses it.
        let mut counter = Seq::default();
        let primer = ev(
            counter.next(),
            turn,
            None,
            None,
            EventKind::UserPrompt {
                text: "primer".to_string(),
                images: Vec::new(),
                queued_prompt: None,
            },
        );
        let primer_id = primer.env.id;
        state.apply(&primer);
        let primed_seq = state.last_seq;

        let duplicate_id = EventId::new(); // reused as `id` on every event below
        let unknown_parent = EventId::new(); // never applied to this state
        let regressed_seq = Seq::default(); // ZERO — below `primed_seq`
        assert!(regressed_seq < primed_seq);
        let subagent_invocation = SubagentInvocationId::new();
        let child_run = RunId::new(SessionId::new(), uuid::Uuid::now_v7());

        let table: Vec<(&str, EventKind)> = vec![
            (
                "user_prompt",
                EventKind::UserPrompt {
                    text: String::new(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
            ),
            (
                "assistant_delta",
                EventKind::AssistantDelta {
                    text: String::new(),
                },
            ),
            (
                "assistant_message",
                EventKind::AssistantMessage {
                    text: String::new(),
                },
            ),
            (
                "reasoning_delta",
                EventKind::ReasoningDelta {
                    text: String::new(),
                },
            ),
            (
                "reasoning_state",
                EventKind::ReasoningState {
                    state: ego_core::generator::types::ReasoningState {
                        blob: String::new(),
                        dialect: String::new(),
                    },
                },
            ),
            (
                "tool_call_started",
                EventKind::ToolCallStarted {
                    name: String::new(),
                    args: String::new(),
                    call_id: None,
                },
            ),
            (
                "tool_call_delta",
                EventKind::ToolCallDelta {
                    text: String::new(),
                },
            ),
            (
                "tool_call_completed",
                EventKind::ToolCallCompleted {
                    ok: false,
                    result: ToolResult::builtin(String::new()),
                },
            ),
            (
                "permission_requested",
                EventKind::PermissionRequested {
                    tool: String::new(),
                    escalation: Escalation::NoRuleCovers {
                        op: OpName::Read,
                        path: CanonPath::new(""),
                    },
                },
            ),
            (
                "permission_amended",
                EventKind::PermissionAmended {
                    tool: String::new(),
                },
            ),
            (
                "permission_decided",
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    by: None,
                    reason: None,
                },
            ),
            (
                "usage_updated",
                EventKind::UsageUpdated {
                    usage: Usage {
                        uncached_input_tokens: u64::MAX,
                        cache_read_tokens: u64::MAX,
                        cache_write_tokens: u64::MAX,
                        cache_write_extended_tokens: u64::MAX,
                        output_tokens: u64::MAX,
                        reasoning_tokens: Some(u64::MAX),
                    },
                    cost_delta: CostDelta::Unknown(
                        ego_core::cost::UnknownCost::ProviderReportedNoUsage,
                    ),
                    attempt: None,
                },
            ),
            (
                "context_window_updated",
                EventKind::ContextWindowUpdated {
                    used_tokens: u64::MAX,
                    provenance: OccupancyProvenance::LocalEstimate,
                    window: None,
                },
            ),
            (
                "turn_completed",
                EventKind::TurnCompleted {
                    ok: false,
                    exit_reason: ExitReason::Provider,
                },
            ),
            (
                "compacted",
                EventKind::Compacted {
                    summary: String::new(),
                },
            ),
            (
                "forked",
                EventKind::Forked {
                    source: SessionId::new(),
                    at_seq: Seq::default(),
                    snapshot: None,
                },
            ),
            (
                "hook_catalogue_admitted",
                EventKind::HookCatalogueAdmitted {
                    descriptors: HookCatalogueEvidence::new(vec![hook_catalogue_entry()]).unwrap(),
                },
            ),
            (
                "hook_lifecycle_reached",
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
            (
                "hook_invocation_started",
                EventKind::HookInvocationStarted {
                    occurrence: duplicate_id,
                    hook: hook_descriptor().id(),
                    invocation: HookInvocationId::parse("01900000-0000-7000-8000-000000000012")
                        .unwrap(),
                    lifecycle: ego_core::event::hook::HookLifecycle::PreToolUse,
                    subject: ego_core::event::hook::HookSubject::Session,
                    activation: ego_core::event::hook::HookActivation::Persistent,
                },
            ),
            (
                "hook_invocation_settled",
                EventKind::HookInvocationSettled {
                    occurrence: EventId::new(),
                    hook: hook_descriptor().id(),
                    invocation: HookInvocationId::parse("01900000-0000-7000-8000-000000000012")
                        .unwrap(),
                    result: ego_core::event::hook::HookResult::Outcome {
                        outcome: HookOutcome::new(HookDecision::Continue, None),
                    },
                },
            ),
            (
                "subagent_requested",
                EventKind::SubagentRequested {
                    invocation: subagent_invocation,
                    task: blob_ref(12),
                    selections: RunSelections::empty(),
                },
            ),
            (
                "subagent_spawned",
                EventKind::SubagentSpawned {
                    invocation: subagent_invocation,
                    child: child_run,
                },
            ),
            (
                "subagent_settled",
                EventKind::SubagentSettled {
                    invocation: subagent_invocation,
                    child: Some(child_run),
                    terminal: SubagentTerminal::Returned {
                        final_message: blob_ref(4),
                    },
                },
            ),
            (
                "error",
                EventKind::Error {
                    message: String::new(),
                    kind: ErrorKind::Unclassified,
                },
            ),
            // The attempt lifecycle, last on purpose: `generation_started`
            // flips attribution on, and every entry above it is written the
            // way a pre-attribution log is. The settlement and the correction
            // below name attempts nobody started, so both are refused —
            // exactly the degenerate case this test is about. A refusal is
            // recorded; it is never a panic.
            (
                "generation_started",
                EventKind::GenerationStarted {
                    attempt: AttemptRef::first(GenerationId::new()),
                    role: GenerationRole::Main,
                    snapshot: None,
                },
            ),
            (
                "execution_observed",
                EventKind::ExecutionObserved {
                    observation: ego_core::permit::ExecObservation::new(
                        ego_core::permit::BackendIdentity::new("test", "v1"),
                        ego_core::permit::ExecCaps::empty(),
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                            .try_into()
                            .unwrap(),
                    ),
                    probe_evidence: None,
                },
            ),
            (
                "attempt_settled",
                EventKind::AttemptSettled {
                    attempt: AttemptRef::first(GenerationId::new()),
                    outcome: Outcome::new(
                        StopReason::Error,
                        DeliveryCertainty::PossiblySent,
                        UsageReport::Missing,
                    ),
                },
            ),
            (
                "usage_corrected",
                EventKind::UsageCorrected {
                    attempt: AttemptRef::first(GenerationId::new()),
                    usage: Usage::IDENTITY,
                    cost_delta: CostDelta::Unknown(ego_core::cost::UnknownCost::Overflow),
                },
            ),
        ];
        assert_eq!(
            table.len(),
            28,
            "keep this table in sync with EventKind's variant count (ARCH §1)"
        );

        for (name, kind) in table {
            assert_eq!(
                kind.name(),
                name,
                "table entry out of sync with EventKind::name()"
            );
            let mut e = DomainEvent {
                env: Envelope {
                    seq: regressed_seq,
                    id: duplicate_id,
                    turn,
                    item: None,
                    parent: Some(unknown_parent),
                    ts: Timestamp::from_millis(1_000),
                    producer: Producer::Main,
                },
                kind,
            };
            if matches!(e.kind, EventKind::HookInvocationSettled { .. }) {
                e.env.producer = Producer::Hook(hook_descriptor().id());
            }
            state.apply(&e); // must not panic, for every variant, under every degenerate axis at once
        }

        assert_eq!(
            state.unresolved_parents.len(),
            28,
            "every event's parent was unresolvable"
        );
        assert_eq!(
            state.last_seq, regressed_seq,
            "apply must not clamp/reject the regressed seq"
        );
        assert_eq!(
            state
                .refusals
                .iter()
                .map(|r| r.reason)
                .collect::<Vec<RefusalReason>>(),
            vec![
                RefusalReason::HookOccurrenceParentMismatch,
                RefusalReason::HookInvocationUnknown,
                RefusalReason::UnknownAttempt,
                RefusalReason::UnknownAttempt,
            ],
            "the hook start has the wrong parent and its settlement has no live invocation, while the settlement and the correction name attempts nobody started, \
             and a refusal is recorded rather than raised"
        );
        assert_eq!(
            state.live_attempts.len(),
            1,
            "only the started attempt is live; the other two never opened one"
        );
        assert!(
            state.items.is_empty(),
            "item: None on every event must touch no item"
        );
        assert_eq!(
            state.known_events,
            std::collections::HashSet::from([primer_id, duplicate_id]),
            "the primer's id and the (repeated) duplicate id — deduped by the HashSet"
        );
        assert_eq!(state.usage.uncached_input_tokens, u64::MAX);
        assert_eq!(state.usage.output_tokens, u64::MAX);
        assert_eq!(
            state.cost,
            CumulativeCost::Unknown(ego_core::cost::UnknownCost::ProviderReportedNoUsage),
            "an unpriced generation must poison the cumulative total, not read as free"
        );
    }

    #[test]
    fn usage_updates_saturate_instead_of_panicking_on_overflow() {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();

        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::UsageUpdated {
                usage: tokens(u64::MAX - 1, u64::MAX - 1),
                cost_delta: priced("0"),
                attempt: None,
            },
        ));
        // Summing past u64::MAX must not panic (dev profile has
        // overflow-checks = true, so an unchecked `+=` would).
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::UsageUpdated {
                usage: tokens(10, 10),
                cost_delta: priced("0"),
                attempt: None,
            },
        ));

        assert_eq!(state.usage.uncached_input_tokens, u64::MAX);
        assert_eq!(state.usage.output_tokens, u64::MAX);
    }

    #[test]
    fn apply_handles_repeated_and_self_referential_event_ids() {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();

        // An event whose declared parent is its own id: at the point the
        // parent is checked, the event's own id is not yet known, so this
        // is (correctly) recorded as unresolved rather than causing a panic
        // or an infinite loop.
        let self_id = EventId::new();
        let self_referential = DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: self_id,
                turn,
                item: None,
                parent: Some(self_id),
                ts: Timestamp::from_millis(1_000),
                producer: Producer::Main,
            },
            kind: EventKind::Error {
                message: "loop".to_string(),
                kind: ErrorKind::Unclassified,
            },
        };
        state.apply(&self_referential);
        assert_eq!(state.unresolved_parents, vec![self_id]);

        // Applying the exact same event a second time (duplicate delivery,
        // or replaying a log segment that was already applied): must not
        // panic. `known_events` (a HashSet) dedupes the id; `errors` (an
        // append log) legitimately records the message twice, since apply
        // has no way to distinguish "duplicate delivery" from "the error
        // genuinely happened twice" without a broader idempotency contract
        // that is out of scope here.
        state.apply(&self_referential);
        assert_eq!(state.known_events.len(), 1);
        assert_eq!(state.errors, vec!["loop".to_string(), "loop".to_string()]);
    }

    #[test]
    fn assistant_deltas_accumulate_into_the_item_text() {
        let turn = TurnId::new();
        let item = ItemId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();
        state.apply(&ev(
            seq.next(),
            turn,
            Some(item),
            None,
            EventKind::AssistantDelta {
                text: "Hel".to_string(),
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            Some(item),
            None,
            EventKind::AssistantDelta {
                text: "lo".to_string(),
            },
        ));
        assert_eq!(state.items[&item].text, "Hello");
    }

    #[test]
    fn assistant_message_replaces_the_item_text() {
        let turn = TurnId::new();
        let item = ItemId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();
        state.apply(&ev(
            seq.next(),
            turn,
            Some(item),
            None,
            EventKind::AssistantDelta {
                text: "partial".to_string(),
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            Some(item),
            None,
            EventKind::AssistantMessage {
                text: "final".to_string(),
            },
        ));
        assert_eq!(state.items[&item].text, "final");
    }

    #[test]
    fn tool_call_lifecycle_updates_the_item() {
        let turn = TurnId::new();
        let item = ItemId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();
        state.apply(&ev(
            seq.next(),
            turn,
            Some(item),
            None,
            EventKind::ToolCallStarted {
                name: "grep".to_string(),
                args: "{\"q\":\"foo\"}".to_string(),
                call_id: Some("call_b".to_string()),
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            Some(item),
            None,
            EventKind::ToolCallDelta {
                text: "partial output".to_string(),
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            Some(item),
            None,
            EventKind::ToolCallCompleted {
                ok: true,
                result: ToolResult::builtin("final output"),
            },
        ));

        let it = &state.items[&item];
        assert_eq!(it.tool_name.as_deref(), Some("grep"));
        assert!(it.text.is_empty());
        assert_eq!(
            it.tool_result.as_ref().map(ToolResult::model_text),
            Some("final output".to_string())
        );
        assert!(it.completed);
        assert_eq!(it.ok, Some(true));
    }

    #[test]
    fn events_without_an_item_id_do_not_panic_and_touch_no_item() {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::AssistantDelta {
                text: "orphaned".to_string(),
            },
        ));
        assert!(state.items.is_empty());
    }

    #[test]
    fn permission_requested_and_decided_are_logged_in_order() {
        let turn = TurnId::new();
        let item = ItemId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();
        state.apply(&ev(
            seq.next(),
            turn,
            Some(item),
            None,
            EventKind::PermissionRequested {
                tool: "edit".to_string(),
                escalation: Escalation::NoRuleCovers {
                    op: OpName::Write,
                    path: CanonPath::new("/ws/a.txt"),
                },
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            Some(item),
            None,
            EventKind::PermissionDecided {
                allowed: Some(true),
                by: Some(Authority::Mode { mode: Mode::Edits }),
                reason: None,
            },
        ));

        assert_eq!(
            state.permissions,
            vec![
                // The escalation, rendered. The reducer keeps a sentence
                // because nothing reads `permissions` for structure; the
                // structured fields stay in the log, which is where the audit
                // reads them.
                PermissionEvent::Requested {
                    item: Some(item),
                    description: "no rule covers write /ws/a.txt".to_string()
                },
                PermissionEvent::Decided {
                    item: Some(item),
                    allowed: true
                },
            ]
        );
    }

    /// Occupancy is a *level*, so the reducer replaces it; usage is a
    /// *flow*, so the reducer adds it. The two live in one test because the
    /// contrast is the point — and because a log with only one occupancy
    /// report cannot tell replace from add at all (`0 + x == x`), which is
    /// exactly how this rule can be broken without any test noticing.
    ///
    /// The second report here is *lower* than the first, which is the real
    /// post-compaction shape: the history was thrown away, so the context
    /// genuinely holds less than it did. An adding reducer would report
    /// 4700 tokens in a window that holds 400.
    #[test]
    fn context_occupancy_replaces_while_usage_accumulates() {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();

        for (used, provenance) in [
            (4_300, OccupancyProvenance::GenerationBoundary),
            (400, OccupancyProvenance::AuthoritativeQuery),
        ] {
            state.apply(&ev(
                seq.next(),
                turn,
                None,
                None,
                EventKind::UsageUpdated {
                    usage: tokens(100, 20),
                    cost_delta: priced("0.001"),
                    attempt: None,
                },
            ));
            state.apply(&ev(
                seq.next(),
                turn,
                None,
                None,
                EventKind::ContextWindowUpdated {
                    used_tokens: used,
                    provenance,
                    window: None,
                },
            ));
        }

        assert_eq!(
            state.context.used_tokens,
            Some(400),
            "the latest occupancy replaces the previous one; 4700 would mean it accumulated"
        );
        assert_eq!(
            state.context.provenance,
            Some(OccupancyProvenance::AuthoritativeQuery),
            "provenance travels with the number it describes"
        );
        // The same two events' token counts *did* accumulate, so this is a
        // contrast within one log rather than two unrelated behaviours.
        assert_eq!(state.usage.uncached_input_tokens, 200);
        assert_eq!(state.usage.output_tokens, 40);
        assert_eq!(
            state.cost,
            CumulativeCost::Known {
                total: usd("0.002")
            }
        );
    }

    #[test]
    fn usage_updates_accumulate() {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::UsageUpdated {
                usage: tokens(10, 2),
                cost_delta: priced("0.25"),
                attempt: None,
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::UsageUpdated {
                usage: tokens(5, 1),
                cost_delta: priced("0.125"),
                attempt: None,
            },
        ));
        assert_eq!(state.usage.uncached_input_tokens, 15);
        assert_eq!(state.usage.output_tokens, 3);
        assert_eq!(
            state.cost,
            CumulativeCost::Known {
                total: usd("0.375")
            },
            "cost deltas accumulate exactly alongside the token totals"
        );
    }

    #[test]
    fn turn_completed_records_outcome() {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::TurnCompleted {
                ok: false,
                exit_reason: ExitReason::Budget,
            },
        ));
        let t = &state.turns[&turn];
        assert!(t.completed);
        assert!(!t.ok);
        assert_eq!(t.exit_reason, Some(ExitReason::Budget));
    }

    #[test]
    fn subagent_hook_compaction_fork_and_error_are_logged() {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();
        let source = SessionId::new();
        let subagent_invocation = SubagentInvocationId::new();
        let child_run = RunId::new(SessionId::new(), uuid::Uuid::now_v7());

        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::SubagentRequested {
                invocation: subagent_invocation,
                task: blob_ref(12),
                selections: RunSelections::empty(),
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::SubagentSpawned {
                invocation: subagent_invocation,
                child: child_run,
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::SubagentSettled {
                invocation: subagent_invocation,
                child: Some(child_run),
                terminal: SubagentTerminal::Returned {
                    final_message: blob_ref(4),
                },
            },
        ));
        let hook = hook_descriptor();
        let invocation = HookInvocationId::new();
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::HookCatalogueAdmitted {
                descriptors: HookCatalogueEvidence::new(vec![HookCatalogueEntryEvidence::new(
                    hook.clone(),
                    HookActivation::Persistent,
                )])
                .unwrap(),
            },
        ));
        let reached = ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::HookLifecycleReached {
                occurrence: HookOccurrenceKey::new(
                    ego_core::event::hook::HookLifecycle::PreToolUse,
                    HookOccurrenceOwner::Session {
                        session: SessionId::new(),
                    },
                ),
                subject: ego_core::event::hook::HookSubject::Session,
                plan: HookPlan::new(vec![HookPlanEntry::new(
                    hook.id(),
                    HookActivation::Persistent,
                )])
                .unwrap(),
            },
        );
        let reached_id = reached.env.id;
        state.apply(&reached);
        let started = ev(
            seq.next(),
            turn,
            None,
            Some(reached_id),
            EventKind::HookInvocationStarted {
                occurrence: reached_id,
                hook: hook.id(),
                invocation,
                lifecycle: ego_core::event::hook::HookLifecycle::PreToolUse,
                subject: ego_core::event::hook::HookSubject::Session,
                activation: ego_core::event::hook::HookActivation::Persistent,
            },
        );
        let started_id = started.env.id;
        state.apply(&started);
        let mut settled = ev(
            seq.next(),
            turn,
            None,
            Some(started_id),
            EventKind::HookInvocationSettled {
                occurrence: reached_id,
                hook: hook.id(),
                invocation,
                result: ego_core::event::hook::HookResult::Outcome {
                    outcome: HookOutcome::new(HookDecision::Block, None),
                },
            },
        );
        settled.env.producer = Producer::Hook(hook.id());
        state.apply(&settled);
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::Compacted {
                summary: "trimmed".to_string(),
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::Forked {
                source,
                at_seq: Seq::default(),
                snapshot: None,
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::Error {
                message: "oops".to_string(),
                kind: ErrorKind::Unclassified,
            },
        ));

        assert_eq!(
            state.subagents,
            vec![
                SubagentEvent::Requested {
                    invocation: subagent_invocation,
                    task: blob_ref(12),
                    selections: RunSelections::empty(),
                },
                SubagentEvent::Spawned {
                    invocation: subagent_invocation,
                    child: child_run,
                },
                SubagentEvent::Settled {
                    invocation: subagent_invocation,
                    child: Some(child_run),
                    terminal: SubagentTerminal::Returned {
                        final_message: blob_ref(4),
                    },
                },
            ]
        );
        assert_eq!(state.hook_catalogue().len(), 1);
        assert_eq!(state.hook_settlements().len(), 1);
        assert_eq!(state.compactions, vec!["trimmed".to_string()]);
        assert_eq!(state.forks.len(), 1);
        assert_eq!(state.forks[0].source, source);
        assert_eq!(state.errors, vec!["oops".to_string()]);
    }

    // --- the attempt ledger ------------------------------------------------
    //
    // See `ARCHITECTURE.md`. The
    // reducer is total, so "refused" means the variant's effect is not
    // applied and a typed `Refusal` is recorded — the envelope bookkeeping
    // still happens either way.

    /// A driver for one session's log: it hands out sequence numbers and
    /// applies as it goes, which is what the harness loop does.
    struct Session {
        turn: TurnId,
        seq: Seq,
        state: SessionState,
    }

    impl Session {
        fn new() -> Self {
            Self {
                turn: TurnId::new(),
                seq: Seq::default(),
                state: SessionState::default(),
            }
        }

        fn apply(&mut self, kind: EventKind) -> EventId {
            let e = ev(self.seq.next(), self.turn, None, None, kind);
            let id = e.env.id;
            self.state.apply(&e);
            id
        }

        fn start(&mut self, attempt: AttemptRef) {
            self.apply(EventKind::GenerationStarted {
                attempt,
                role: GenerationRole::Main,
                snapshot: None,
            });
        }

        fn book(&mut self, attempt: Option<AttemptRef>, usage: Usage, cost: CostDelta) {
            self.apply(EventKind::UsageUpdated {
                usage,
                cost_delta: cost,
                attempt,
            });
        }

        fn settle(&mut self, attempt: AttemptRef, usage: UsageReport) {
            self.apply(EventKind::AttemptSettled {
                attempt,
                outcome: Outcome::new(StopReason::Stop, DeliveryCertainty::PossiblySent, usage),
            });
        }

        fn correct(&mut self, attempt: AttemptRef, usage: Usage, cost: CostDelta) {
            self.apply(EventKind::UsageCorrected {
                attempt,
                usage,
                cost_delta: cost,
            });
        }

        fn reasons(&self) -> Vec<RefusalReason> {
            self.state.refusals.iter().map(|r| r.reason).collect()
        }
    }

    /// Start, book and settle one attempt with `usage` priced at `cost`.
    fn one_generation(s: &mut Session, usage: Usage, cost: CostDelta) -> AttemptRef {
        let attempt = AttemptRef::first(GenerationId::new());
        s.start(attempt);
        s.book(Some(attempt), usage, cost);
        s.settle(attempt, UsageReport::Reported(usage));
        attempt
    }

    #[test]
    fn an_attempt_is_live_from_start_to_settlement_and_no_longer() {
        let mut s = Session::new();
        let attempt = AttemptRef::first(GenerationId::new());

        s.start(attempt);
        assert_eq!(s.state.live_attempts.len(), 1);
        assert!(s.state.settled_attempts.is_empty());

        s.settle(attempt, UsageReport::Missing);
        assert!(
            s.state.live_attempts.is_empty(),
            "settlement closes the lifecycle; nothing about it is kept live"
        );
        assert_eq!(
            s.state.settled_attempts.len(),
            1,
            "but it is remembered, because refusing an unknown attempt means \
             knowing which ones are known"
        );
        assert!(s.reasons().is_empty());
    }

    /// The bound the acceptance criterion asks for, stated as an experiment:
    /// a long log with many generations, only one of which is ever in flight.
    #[test]
    fn live_attempt_state_is_bounded_by_concurrent_generations_not_by_log_length() {
        let mut s = Session::new();
        for _ in 0..50 {
            let attempt = one_generation(&mut s, tokens(10, 5), priced("0.01"));
            assert_eq!(
                s.state.live_attempts.len(),
                0,
                "no generation stays live past its settlement: {attempt:?}"
            );
            // Events per generation, not events per log: three here, and the
            // live map does not grow with either.
            assert!(s.state.live_attempts.len() <= 1);
        }
        assert_eq!(s.state.charges.len(), 50, "the ledger does grow, by design");
        assert_eq!(s.state.cost, CumulativeCost::Known { total: usd("0.5") });
    }

    #[test]
    fn two_generations_can_be_in_flight_at_once() {
        let mut s = Session::new();
        let a = AttemptRef::first(GenerationId::new());
        let b = AttemptRef::first(GenerationId::new());
        s.start(a);
        s.start(b);
        assert_eq!(s.state.live_attempts.len(), 2);

        s.book(Some(a), tokens(10, 1), priced("0.01"));
        s.book(Some(b), tokens(20, 2), priced("0.02"));
        s.settle(b, UsageReport::Reported(tokens(20, 2)));

        assert_eq!(s.state.live_attempts.len(), 1, "a is still in flight");
        assert_eq!(s.state.cost, CumulativeCost::Known { total: usd("0.03") });
        assert!(s.reasons().is_empty());
    }

    #[test]
    fn a_settlement_for_an_attempt_nobody_started_is_refused() {
        let mut s = Session::new();
        s.settle(AttemptRef::first(GenerationId::new()), UsageReport::Missing);

        assert_eq!(s.reasons(), vec![RefusalReason::UnknownAttempt]);
        assert!(s.state.settled_attempts.is_empty());
    }

    /// The reason `AttemptRef` travels whole rather than a bare `AttemptId`.
    #[test]
    fn a_settlement_naming_a_different_generation_is_refused_and_leaves_the_attempt_in_flight() {
        let mut s = Session::new();
        let real = AttemptRef::first(GenerationId::new());
        s.start(real);

        let mut imposter = AttemptRef::first(GenerationId::new());
        // Same attempt id, different generation — the swap a bare `AttemptId`
        // could not have noticed.
        imposter = rebind(imposter, real.attempt());
        s.settle(imposter, UsageReport::Missing);

        assert_eq!(s.reasons(), vec![RefusalReason::GenerationMismatch]);
        assert_eq!(
            s.state.live_attempts.len(),
            1,
            "the real attempt is still in flight: a settlement naming the \
             wrong generation says nothing about it, so closing it on that \
             evidence would strand it"
        );
    }

    #[test]
    fn settling_or_starting_the_same_attempt_twice_is_refused() {
        let mut s = Session::new();
        let attempt = AttemptRef::first(GenerationId::new());
        s.start(attempt);
        s.start(attempt);
        s.settle(attempt, UsageReport::Missing);
        s.settle(attempt, UsageReport::Missing);
        s.start(attempt);

        assert_eq!(
            s.reasons(),
            vec![
                RefusalReason::AlreadyStarted,
                RefusalReason::AlreadySettled,
                RefusalReason::AlreadySettled,
            ]
        );
        assert_eq!(s.state.settled_attempts.len(), 1);
    }

    #[test]
    fn usage_attributed_to_an_unknown_attempt_is_refused_and_costs_nothing() {
        let mut s = Session::new();
        s.start(AttemptRef::first(GenerationId::new()));
        s.book(
            Some(AttemptRef::first(GenerationId::new())),
            tokens(999, 999),
            priced("99"),
        );

        assert_eq!(s.reasons(), vec![RefusalReason::UnknownAttempt]);
        assert!(s.state.charges.is_empty(), "nothing was booked");
        assert_eq!(s.state.cost, CumulativeCost::Unstarted);
    }

    /// Unattributed usage is accepted only while nothing has shown it knows
    /// how to attribute. Otherwise a producer opts out by omitting a field.
    #[test]
    fn unattributed_usage_is_legacy_only_and_refused_once_attribution_has_begun() {
        let mut s = Session::new();
        s.book(None, tokens(10, 1), priced("0.01"));
        assert_eq!(s.state.cost, CumulativeCost::Known { total: usd("0.01") });
        assert!(
            s.reasons().is_empty(),
            "a pre-attribution log still reduces"
        );

        s.start(AttemptRef::first(GenerationId::new()));
        s.book(None, tokens(10, 1), priced("0.01"));

        assert_eq!(s.reasons(), vec![RefusalReason::UnattributedUsage]);
        assert_eq!(
            s.state.cost,
            CumulativeCost::Known { total: usd("0.01") },
            "the refused figure did not move the total"
        );
    }

    #[test]
    fn usage_booked_after_settlement_is_refused_because_a_correction_is_the_only_way_back() {
        let mut s = Session::new();
        let attempt = one_generation(&mut s, tokens(10, 1), priced("0.01"));
        s.book(Some(attempt), tokens(10, 1), priced("0.01"));

        assert_eq!(s.reasons(), vec![RefusalReason::AlreadySettled]);
        assert_eq!(
            s.state.cost,
            CumulativeCost::Known { total: usd("0.01") },
            "the double count this contract exists to prevent"
        );
    }

    /// The settlement's own `UsageReport` is evidence about the bookings, not
    /// a booking. A disagreement is recorded and the money does not move.
    #[test]
    fn a_settlement_disagreeing_with_what_was_booked_is_recorded_and_still_settles() {
        let mut s = Session::new();
        let attempt = AttemptRef::first(GenerationId::new());
        s.start(attempt);
        s.book(Some(attempt), tokens(10, 1), priced("0.01"));
        s.settle(attempt, UsageReport::Reported(tokens(999, 999)));

        assert_eq!(
            s.reasons(),
            vec![RefusalReason::UsageDisagreesWithSettlement]
        );
        assert!(
            s.state.live_attempts.is_empty() && s.state.settled_attempts.len() == 1,
            "it settles anyway: the bookings are what a real gate was charged"
        );
        assert_eq!(s.state.usage, tokens(10, 1));
        assert_eq!(s.state.cost, CumulativeCost::Known { total: usd("0.01") });
    }

    #[test]
    fn a_correction_replaces_one_contribution_and_leaves_the_others_alone() {
        let mut s = Session::new();
        let first = one_generation(&mut s, tokens(10, 1), priced("0.01"));
        one_generation(&mut s, tokens(20, 2), priced("0.02"));
        assert_eq!(s.state.cost, CumulativeCost::Known { total: usd("0.03") });

        s.correct(first, tokens(100, 10), priced("0.10"));

        assert!(s.reasons().is_empty());
        assert_eq!(
            s.state.cost,
            CumulativeCost::Known { total: usd("0.12") },
            "0.10 replaced 0.01; the other generation's 0.02 is untouched"
        );
        assert_eq!(s.state.usage, tokens(120, 12));
        assert_eq!(s.state.charges.len(), 2, "replaced, not appended");
    }

    /// The correction that matters most in practice, and the reason this is
    /// a refold rather than a subtraction: nobody knows how big an `Unknown`
    /// was, so there is nothing to subtract.
    #[test]
    fn a_correction_clears_a_total_that_a_missing_usage_report_poisoned() {
        let mut s = Session::new();
        let silent = AttemptRef::first(GenerationId::new());
        s.start(silent);
        s.book(
            Some(silent),
            Usage::IDENTITY,
            CostDelta::Unknown(ego_core::cost::UnknownCost::ProviderReportedNoUsage),
        );
        s.settle(silent, UsageReport::Reported(Usage::IDENTITY));
        one_generation(&mut s, tokens(20, 2), priced("0.02"));

        assert_eq!(
            s.state.cost,
            CumulativeCost::Unknown(ego_core::cost::UnknownCost::ProviderReportedNoUsage),
            "one unpriced generation poisons the total, and a later priced \
             one cannot repair a sum with a hole in it"
        );

        s.correct(silent, tokens(10, 1), priced("0.01"));

        assert_eq!(
            s.state.cost,
            CumulativeCost::Known { total: usd("0.03") },
            "the billing figure arrived, so the hole closed and the total is \
             a number again"
        );
        assert_eq!(s.state.usage, tokens(30, 3));
    }

    /// The ledger is a `Vec` and not a map because `add_delta` is
    /// order-dependent: the first `Known` delta fixes the settlement
    /// currency. Folded EUR-then-USD this totals in EUR; folded the other way
    /// round the EUR contribution has no quote and poisons the total.
    #[test]
    fn a_refold_preserves_ledger_order_so_the_settlement_currency_cannot_move() {
        let mut s = Session::new();
        let eur = one_generation(&mut s, tokens(10, 1), eur_priced("1"));
        one_generation(&mut s, tokens(20, 2), usd_priced_with_eur_quote("1", "0.9"));

        assert_eq!(
            s.state.cost,
            CumulativeCost::Known {
                total: money("1.9", "EUR")
            },
            "EUR came first, so EUR settles"
        );

        s.correct(eur, tokens(10, 1), eur_priced("2"));

        assert_eq!(
            s.state.cost,
            CumulativeCost::Known {
                total: money("2.9", "EUR")
            },
            "the corrected entry keeps its position, so EUR still settles. \
             Refolded in any other order the USD entry would establish the \
             currency and the EUR one would poison the total"
        );
    }

    #[test]
    fn a_correction_for_an_unknown_or_still_live_attempt_is_refused() {
        let mut s = Session::new();
        let live = AttemptRef::first(GenerationId::new());
        s.start(live);
        s.book(Some(live), tokens(10, 1), priced("0.01"));

        s.correct(live, tokens(99, 9), priced("9"));
        s.correct(
            AttemptRef::first(GenerationId::new()),
            tokens(99, 9),
            priced("9"),
        );

        assert_eq!(
            s.reasons(),
            vec![RefusalReason::NotSettled, RefusalReason::UnknownAttempt]
        );
        assert_eq!(s.state.cost, CumulativeCost::Known { total: usd("0.01") });
    }

    /// An attempt that settled without booking anything has no contribution
    /// to replace, and a correction must not conjure one.
    #[test]
    fn a_correction_for_an_attempt_that_booked_nothing_is_refused() {
        let mut s = Session::new();
        let attempt = AttemptRef::first(GenerationId::new());
        s.start(attempt);
        s.settle(attempt, UsageReport::Missing);

        s.correct(attempt, tokens(10, 1), priced("0.01"));

        assert_eq!(s.reasons(), vec![RefusalReason::UnknownAttempt]);
        assert_eq!(s.state.cost, CumulativeCost::Unstarted);
        assert!(s.state.charges.is_empty());
    }

    /// Several increments for one attempt collapse into the revised figure —
    /// a correction states what the attempt used, not what to add to it.
    #[test]
    fn a_correction_collapses_every_increment_that_attempt_booked() {
        let mut s = Session::new();
        let attempt = AttemptRef::first(GenerationId::new());
        s.start(attempt);
        s.book(Some(attempt), tokens(10, 1), priced("0.01"));
        s.book(Some(attempt), tokens(5, 1), priced("0.005"));
        s.settle(attempt, UsageReport::Reported(tokens(15, 2)));
        assert_eq!(s.state.charges.len(), 2);
        assert_eq!(
            s.state.cost,
            CumulativeCost::Known {
                total: usd("0.015")
            }
        );

        s.correct(attempt, tokens(20, 3), priced("0.02"));

        assert!(s.reasons().is_empty());
        assert_eq!(s.state.charges.len(), 1, "two increments, one revision");
        assert_eq!(s.state.usage, tokens(20, 3));
        assert_eq!(s.state.cost, CumulativeCost::Known { total: usd("0.02") });
    }

    /// The property the whole ledger rests on: the totals are the fold of it.
    #[test]
    fn the_totals_are_always_the_fold_of_the_ledger() {
        let mut s = Session::new();
        let a = one_generation(&mut s, tokens(10, 1), priced("0.01"));
        one_generation(&mut s, tokens(20, 2), priced("0.02"));
        s.correct(a, tokens(7, 7), priced("0.07"));

        let mut usage = Usage::IDENTITY;
        let mut cost = CumulativeCost::default();
        for charge in &s.state.charges {
            usage.add(&charge.usage);
            cost.add_delta(&charge.cost);
        }
        assert_eq!(s.state.usage, usage);
        assert_eq!(s.state.cost, cost);
    }

    /// `IDENTITY` is the identity of `add`, in both directions and on every
    /// field — the property that lets any fold start there.
    #[test]
    fn the_identity_adds_nothing_to_a_usage_and_a_usage_adds_nothing_to_it() {
        let measured = Usage {
            uncached_input_tokens: 120,
            cache_read_tokens: 900,
            cache_write_tokens: 60,
            cache_write_extended_tokens: 30,
            output_tokens: 45,
            reasoning_tokens: Some(40),
        };

        let mut left = measured;
        left.add(&Usage::IDENTITY);
        assert_eq!(left, measured, "adding the identity changed the total");

        let mut right = Usage::IDENTITY;
        right.add(&measured);
        assert_eq!(right, measured, "the identity was not a clean seed");
    }

    /// Why `IDENTITY`'s `reasoning_tokens` is `Some(0)` and not `None`.
    ///
    /// `add` treats unknown as absorbing, so a `None` seed would make every
    /// total in the process unknown regardless of what providers reported.
    /// This is the test that fails if somebody "simplifies" the const back to
    /// a `None`, or restores a `#[derive(Default)]` that computes one.
    #[test]
    fn a_none_seed_would_poison_every_total_which_is_why_the_identity_is_a_zero() {
        assert_eq!(
            Usage::IDENTITY.reasoning_tokens,
            Some(0),
            "the fold identity must be a measured zero, not an unknown"
        );

        let reported = Usage {
            reasoning_tokens: Some(40),
            ..Usage::IDENTITY
        };
        let mut from_identity = Usage::IDENTITY;
        from_identity.add(&reported);
        assert_eq!(
            from_identity.reasoning_tokens,
            Some(40),
            "a fold seeded at the identity must keep what the provider said"
        );

        let mut from_unknown = Usage {
            reasoning_tokens: None,
            ..Usage::IDENTITY
        };
        from_unknown.add(&reported);
        assert_eq!(
            from_unknown.reasoning_tokens, None,
            "unknown must absorb: a total that summed an unreported \
             generation is not a known figure"
        );
    }

    #[test]
    fn replaying_a_corrected_log_is_deterministic() {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let attempt = AttemptRef::first(GenerationId::new());
        let other = AttemptRef::first(GenerationId::new());
        let log: Vec<DomainEvent> = vec![
            EventKind::GenerationStarted {
                attempt,
                role: GenerationRole::Main,
                snapshot: None,
            },
            EventKind::UsageUpdated {
                usage: tokens(10, 1),
                cost_delta: priced("0.01"),
                attempt: Some(attempt),
            },
            EventKind::AttemptSettled {
                attempt,
                outcome: Outcome::new(
                    StopReason::Stop,
                    DeliveryCertainty::PossiblySent,
                    UsageReport::Reported(tokens(10, 1)),
                ),
            },
            EventKind::GenerationStarted {
                attempt: other,
                role: GenerationRole::Main,
                snapshot: None,
            },
            EventKind::UsageUpdated {
                usage: tokens(20, 2),
                cost_delta: priced("0.02"),
                attempt: Some(other),
            },
            EventKind::UsageCorrected {
                attempt,
                usage: tokens(5, 5),
                cost_delta: priced("0.05"),
            },
        ]
        .into_iter()
        .map(|kind| ev(seq.next(), turn, None, None, kind))
        .collect();

        let reduce = || {
            let mut state = SessionState::default();
            for e in &log {
                state.apply(e);
            }
            state
        };
        assert_eq!(reduce(), reduce());
        assert_eq!(reduce().cost, CumulativeCost::Known { total: usd("0.07") });
    }

    /// Same attempt id, different generation — the swap a bare `AttemptId`
    /// could not have noticed. There is deliberately no public constructor
    /// for this, so the test builds it through serde, which is also how a
    /// forged record would arrive.
    fn rebind(reference: AttemptRef, attempt: AttemptId) -> AttemptRef {
        let json = serde_json::json!({
            "generation": reference.generation(),
            "attempt": attempt,
        });
        serde_json::from_value(json).expect("an AttemptRef is two ids")
    }

    fn money(decimal: &str, currency: &str) -> Money {
        Money::new(
            decimal.parse::<Amount>().expect("a valid test amount"),
            CurrencyCode::new(currency).expect("a valid test currency"),
        )
    }

    fn eur_priced(decimal: &str) -> CostDelta {
        CostDelta::Known {
            amount: money(decimal, "EUR"),
            attribution: CostAttribution::Table {
                revision: PricingRevision("test-2026-08".to_string()),
            },
            fx: None,
        }
    }

    /// A USD cost carrying its own persisted USD→EUR quote, so it can settle
    /// into a EUR session.
    fn usd_priced_with_eur_quote(decimal: &str, rate: &str) -> CostDelta {
        CostDelta::Known {
            amount: money(decimal, "USD"),
            attribution: CostAttribution::Table {
                revision: PricingRevision("test-2026-08".to_string()),
            },
            fx: Some(FxQuote {
                from: CurrencyCode::new("USD").expect("USD is a valid code"),
                to: CurrencyCode::new("EUR").expect("EUR is a valid code"),
                rate: rate.parse::<Amount>().expect("a valid test rate"),
                quoted_at: Timestamp::from_millis(1_000),
            }),
        }
    }

    /// A pause travels through `Running -> Requested -> Paused` and only a
    /// resume leaves the last state.
    ///
    /// The middle state is the contract, not an implementation detail: the
    /// ADR requires `_ego/pause` to answer `pending` until the boundary
    /// exists, and a two-state model has no way to say that.
    #[test]
    fn a_pause_reaches_its_boundary_through_a_pending_state() {
        let turn = TurnId::new();
        let request = request_id(1);
        let mut seq = Seq::default();
        let mut state = SessionState::default();
        assert_eq!(state.pause, PauseState::Running);
        assert!(state.pause.admits_effects());

        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::SessionPauseRequested { request },
        ));
        assert_eq!(state.pause, PauseState::Requested { request });
        assert!(
            !state.pause.admits_effects(),
            "admission closes when the pause is asked for, not when it lands"
        );

        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::SessionPaused { request },
        ));
        assert_eq!(state.pause, PauseState::Paused { request });
        assert_eq!(state.pause.request(), Some(request));

        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::SessionResumed {
                request: request_id(2),
            },
        ));
        assert_eq!(state.pause, PauseState::Running);
        assert!(state.pause.admits_effects());
    }

    /// A second pause request leaves the first one standing.
    ///
    /// The first request is the one the boundary will answer. Overwriting it
    /// would make the eventual `SessionPaused` name a request that did not
    /// start this hold, and tell the first host its own pause never landed.
    #[test]
    fn a_second_pause_request_does_not_displace_the_one_in_flight() {
        let turn = TurnId::new();
        let (first, second) = (request_id(1), request_id(2));
        let mut seq = Seq::default();
        let mut state = SessionState::default();
        for request in [first, second] {
            state.apply(&ev(
                seq.next(),
                turn,
                None,
                None,
                EventKind::SessionPauseRequested { request },
            ));
        }
        assert_eq!(state.pause, PauseState::Requested { request: first });

        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::SessionPaused { request: second },
        ));
        assert_eq!(
            state.pause,
            PauseState::Requested { request: first },
            "a boundary answered a request that is not the one holding this session"
        );
    }

    /// A boundary nothing asked for is ignored.
    ///
    /// It can only come from a log that lost its request, and reducing it
    /// would manufacture the boundary the ADR says is never manufactured.
    #[test]
    fn a_boundary_without_a_request_does_not_pause_a_running_session() {
        let mut state = SessionState::default();
        state.apply(&ev(
            Seq::default().next(),
            TurnId::new(),
            None,
            None,
            EventKind::SessionPaused {
                request: request_id(1),
            },
        ));
        assert_eq!(state.pause, PauseState::Running);
    }

    /// A resume releases a pause that never reached its boundary.
    ///
    /// Refusing to release a pending pause would leave a session nothing
    /// could free: the boundary may never arrive if the effect that was in
    /// flight died with the process.
    #[test]
    fn a_resume_releases_a_pause_that_is_still_pending() {
        let turn = TurnId::new();
        let mut seq = Seq::default();
        let mut state = SessionState::default();
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::SessionPauseRequested {
                request: request_id(1),
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::SessionResumed {
                request: request_id(2),
            },
        ));
        assert_eq!(state.pause, PauseState::Running);
    }

    /// Distinct, stable request ids, without pulling an id generator into
    /// tests that are about ordering rather than identity.
    fn request_id(n: u128) -> RequestId {
        RequestId::from_uuid(uuid::Uuid::from_u128(n))
    }
}
