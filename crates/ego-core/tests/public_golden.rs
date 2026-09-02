//! `PublicEvent` projection: golden fixtures for the privacy-filtered,
//! versioned JSONL output contract.
//!
//! See `ARCHITECTURE.md` §1, `CLI.md` §4.2 and `plans/m0a-contracts.md`
//! Step 8. Fixtures live in `tests/golden/` as committed data files, not
//! inline string literals, so a change to the projection shows up as a
//! reviewable diff in a data file rather than a diff in test code.

use ego_core::budget::{BudgetGate, Metering, SettlementSource};
use ego_core::cost::{
    Amount, CostAttribution, CostDelta, CurrencyCode, MeteringRevision, Money, PricingRevision,
    ProviderCostRevision, RateSource, UnknownCost,
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
use ego_core::event::id::{
    EventId, ItemId, Producer, Seq, SessionId, SubagentInvocationId, TurnId,
};
use ego_core::event::time::Timestamp;
use ego_core::generator::types::{ErrorKind, ReasoningState};
use ego_core::permit::{Authority, CanonPath, Escalation, OpName, ToolResult};
use ego_core::public::{
    InstructionDescriptor, InstructionSource, ItemKind, PUBLIC_EVENT_VERSION, PublicEvent,
    project_public,
};
use ego_core::run::RunId;
use ego_core::state::Usage;
use ego_core::store::blob::{BlobId, BlobRef};

mod common;

// Wrapped in a module named `public_golden` so `cargo test -p ego-core
// public_golden` (the story's VERIFY command) matches these tests by
// substring, the same trick `tests/reducer.rs` uses for `reducer`: cargo's
// test filter matches on the full test path, not the source file name.
mod public_golden {
    use super::*;

    use super::common::id;

    const SESSION: &str = "00000000-0000-0000-0000-000000000001";

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

    /// Tokens across every priced category, so the golden pins each of them
    /// rather than only the two the old input/output pair had.
    ///
    /// The reasoning split is deliberately left unknown: this fixture stands
    /// for a row that publishes no breakdown, and the golden is therefore
    /// also the pin on how *unknown* crosses the public boundary — which a
    /// zero would have quietly replaced with a claim.
    fn tokens(uncached_input: u64, cache_read: u64, cache_write: u64, output: u64) -> Usage {
        Usage {
            uncached_input_tokens: uncached_input,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            cache_write_extended_tokens: 0,
            output_tokens: output,
            reasoning_tokens: None,
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
    const TURN_1: &str = "00000000-0000-0000-0000-000000000002";
    const TURN_2: &str = "00000000-0000-0000-0000-000000000003";
    const ITEM_USER_1: &str = "00000000-0000-0000-0000-000000000004";
    const ITEM_ASSISTANT_1: &str = "00000000-0000-0000-0000-000000000005";
    const ITEM_TOOL_1: &str = "00000000-0000-0000-0000-000000000006";
    const ITEM_PERM_1: &str = "00000000-0000-0000-0000-000000000007";
    const ITEM_SUB_1: &str = "00000000-0000-0000-0000-000000000008";
    const ITEM_USER_2: &str = "00000000-0000-0000-0000-000000000009";
    const ITEM_ASSISTANT_2: &str = "00000000-0000-0000-0000-00000000000a";

    fn blob_ref(bytes: u64) -> BlobRef {
        BlobRef::new(
            BlobId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap(),
            bytes,
        )
    }
    const ITEM_REASONING_1: &str = "00000000-0000-0000-0000-00000000000b";

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

    /// A realistic two-turn log of 24 events covering 20 of the 26
    /// `EventKind` variants: `UserPrompt`, `AssistantDelta`,
    /// `AssistantMessage` and `TurnCompleted` each appear more than once.
    /// The first turn covers every public item kind (message, tool,
    /// permission, subagent), a `UsageUpdated` (public, as `usage_delta` —
    /// never a running total), a classified `Error`, and the seven
    /// internal-only kinds (`Compacted`, `Forked`, all four hook facts, and
    /// `ReasoningState`) that must not appear in the projection. The second,
    /// final turn is deliberately minimal so the golden's last line is a
    /// plain `turn_completed` — proving "the final result rides the terminal
    /// event" needs no separate mechanism.
    ///
    /// The four kinds this log does not reach: `ReasoningDelta`, covered by
    /// `reasoning_delta_projects_to_an_item_delta_of_kind_reasoning` below,
    /// and `GenerationStarted`, `AttemptSettled` and `UsageCorrected`, which
    /// are the attempt-lifecycle kinds the projection drops wholesale.
    fn fixture_log() -> Vec<DomainEvent> {
        let turn1: TurnId = id(TURN_1);
        let turn2: TurnId = id(TURN_2);
        let item_user_1: ItemId = id(ITEM_USER_1);
        let item_assistant_1: ItemId = id(ITEM_ASSISTANT_1);
        let item_tool_1: ItemId = id(ITEM_TOOL_1);
        let item_perm_1: ItemId = id(ITEM_PERM_1);
        let item_sub_1: ItemId = id(ITEM_SUB_1);
        let item_user_2: ItemId = id(ITEM_USER_2);
        let item_assistant_2: ItemId = id(ITEM_ASSISTANT_2);
        let subagent_invocation = SubagentInvocationId::new();
        let child_run = RunId::new(SessionId::new(), uuid::Uuid::now_v7());
        let item_reasoning_1: ItemId = id(ITEM_REASONING_1);

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
                    usage: tokens(120, 900, 60, 45),
                    cost_delta: priced("0.0027"),
                    attempt: None,
                },
            ),
            ev(
                seq.next(),
                turn1,
                None,
                EventKind::ContextWindowUpdated {
                    used_tokens: 1_125,
                    provenance: OccupancyProvenance::GenerationBoundary,
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
                        ego_core::event::hook::HookLifecycle::PostToolUse,
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
                EventKind::HookInvocationStarted {
                    occurrence: ego_core::event::id::EventId::new(),
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
                    lifecycle: ego_core::event::hook::HookLifecycle::PostToolUse,
                    subject: ego_core::event::hook::HookSubject::Session,
                    activation: ego_core::event::hook::HookActivation::Persistent,
                },
            ),
            ev(
                seq.next(),
                turn1,
                None,
                EventKind::HookInvocationSettled {
                    occurrence: ego_core::event::id::EventId::new(),
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
            ),
            // The blob is provider-private, and the golden is the pin that
            // says so: no line of the committed `.jsonl` contains it.
            ev(
                seq.next(),
                turn1,
                Some(item_reasoning_1),
                EventKind::ReasoningState {
                    state: ReasoningState {
                        blob: "provider-private-continuation-token".to_string(),
                        dialect: "anthropic".to_string(),
                    },
                },
            ),
            ev(
                seq.next(),
                turn1,
                None,
                EventKind::Error {
                    message: "rate limited, retrying".to_string(),
                    kind: ErrorKind::Transient,
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

    /// The core projection golden: `project_public` over the whole fixture
    /// log, `None`s filtered out, one compact JSON object per line — must
    /// match the committed `.jsonl` fixture byte for byte.
    #[test]
    fn projects_fixture_log_to_committed_jsonl() {
        let expected = include_str!("golden/session.jsonl");
        let actual: String = fixture_log()
            .iter()
            .filter_map(project_public)
            .map(|event| serde_json::to_string(&event).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(actual, expected.trim_end());
    }

    /// Explicit, kind-named proof that internal-only events never leak into
    /// the public stream — not just that the golden file happens to omit
    /// them, but that each one individually projects to `None`.
    #[test]
    fn internal_only_events_project_to_none() {
        let internal_kinds = fixture_log()
            .into_iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    EventKind::Compacted { .. }
                        | EventKind::Forked { .. }
                        | EventKind::HookCatalogueAdmitted { .. }
                        | EventKind::HookLifecycleReached { .. }
                        | EventKind::HookInvocationStarted { .. }
                        | EventKind::HookInvocationSettled { .. }
                        | EventKind::ReasoningState { .. }
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            internal_kinds.len(),
            7,
            "fixture should exercise all seven internal-only kinds"
        );
        for event in &internal_kinds {
            assert_eq!(
                project_public(event),
                None,
                "{:?} must not appear in the public stream",
                event.kind
            );
        }
    }

    /// The continuation blob is provider-private, and the check is on the
    /// bytes rather than on the variant. `ReasoningState` projecting to
    /// `None` is how the blob stays out *today*; a future projection that
    /// published the reasoning item and folded the state into it would keep
    /// that `None` and still leak. Searching the committed stream for the
    /// blob catches the leak whatever route it takes to get there.
    #[test]
    fn the_continuation_blob_appears_nowhere_in_the_public_stream() {
        let blob = fixture_log()
            .into_iter()
            .find_map(|e| match e.kind {
                EventKind::ReasoningState { state } => Some(state.blob),
                _ => None,
            })
            .expect("the fixture carries reasoning state");
        assert!(
            !include_str!("golden/session.jsonl").contains(&blob),
            "the provider-private blob reached the public stream"
        );
    }

    /// `SessionStarted` is the header event: the schema version, a
    /// `capabilities` array, and `mcp_errors`/`hook_errors`/`instructions`
    /// omitted entirely (not emitted as `[]`) when there's nothing to report.
    /// It has no `DomainEvent` counterpart — `project_public` never returns
    /// it, the harness that drives a session constructs it directly — so this
    /// golden proves the DTO's own shape and serialisation rather than a
    /// projection.
    #[test]
    fn session_started_header_is_versioned_and_omits_empty_error_arrays() {
        let session: SessionId = id(SESSION);
        let event = PublicEvent::SessionStarted {
            v: PUBLIC_EVENT_VERSION,
            session,
            capabilities: vec![
                "mcp".to_string(),
                "hooks".to_string(),
                "subagents".to_string(),
            ],
            mcp_errors: Vec::new(),
            hook_errors: Vec::new(),
            instructions: Vec::new(),
        };
        let actual = serde_json::to_string(&event).unwrap();
        assert_eq!(
            actual,
            include_str!("golden/session_started.json").trim_end()
        );
    }

    /// The same header event, but with non-empty `mcp_errors`/`hook_errors`
    /// — proving the arrays are present (not just correctly typed) when
    /// there is something to report. A CI gate on this contract is a
    /// presence check, not a length check.
    #[test]
    fn session_started_error_arrays_present_when_nonempty() {
        let session: SessionId = id(SESSION);
        let event = PublicEvent::SessionStarted {
            v: PUBLIC_EVENT_VERSION,
            session,
            capabilities: vec!["mcp".to_string()],
            mcp_errors: vec!["server 'foo' failed to start".to_string()],
            hook_errors: vec!["hook 'pre-commit' malformed".to_string()],
            instructions: Vec::new(),
        };
        let actual = serde_json::to_string(&event).unwrap();
        assert_eq!(
            actual,
            include_str!("golden/session_started_with_errors.json").trim_end()
        );
    }

    /// The header carries what the workspace handed the model — path, digest,
    /// size, scope and how it was chosen — and **no text**. The golden is the
    /// enforcement: a field carrying instruction bodies would change these
    /// bytes, and this is the stream a CI job archives.
    #[test]
    fn session_started_describes_its_instructions_and_never_quotes_them() {
        let session: SessionId = id(SESSION);
        let event = PublicEvent::SessionStarted {
            v: PUBLIC_EVENT_VERSION,
            session,
            capabilities: vec!["instructions".to_string()],
            mcp_errors: Vec::new(),
            hook_errors: Vec::new(),
            instructions: vec![
                InstructionDescriptor {
                    path: "/ws/AGENTS.md".to_string(),
                    sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                        .to_string(),
                    bytes: 0,
                    scope: ".".to_string(),
                    source: InstructionSource::Ambient,
                },
                InstructionDescriptor {
                    path: "/ws/crates/AGENTS.md".to_string(),
                    sha256: "8ff9a4b2c1e0d3f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9"
                        .to_string(),
                    bytes: 42,
                    scope: "crates".to_string(),
                    source: InstructionSource::Ambient,
                },
            ],
        };
        let actual = serde_json::to_string(&event).unwrap();
        assert_eq!(
            actual,
            include_str!("golden/session_started_with_instructions.json").trim_end()
        );
    }

    /// `MeteringSelected`, the third event with no `DomainEvent` counterpart.
    ///
    /// The serialised shape is pinned per case rather than once, because its
    /// whole job is to let a host tell the arrangements apart: `metering`
    /// says whether a monetary budget can bind at all, `ceiling` says what
    /// the configured limit actually does to *this* generation, and `rates`
    /// says what that ceiling rests on. A host reading only `metering` would
    /// report a `--budget` as in force on a subscription row it can never
    /// bind.
    #[test]
    fn metering_selected_distinguishes_all_four_arrangements() {
        let turn: TurnId = id(TURN_1);
        let usd = |decimal: &str| {
            Money::new(
                decimal.parse::<Amount>().expect("a valid test amount"),
                CurrencyCode::new("USD").expect("USD is a valid code"),
            )
        };
        let currency = || Metering::Currency {
            settlement: SettlementSource::Quote,
        };
        let invoiced = || Metering::Currency {
            settlement: SettlementSource::ProviderReported {
                revision: ProviderCostRevision {
                    provider: "xai".to_string(),
                    field: "usage.cost_in_usd_ticks".to_string(),
                    schema_revision: "2026-08-21".to_string(),
                },
            },
        };
        // The ledgers are taken from real gates rather than built by hand, so
        // the golden pins what a session actually publishes.
        let unlimited = BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD is valid"));
        let limited = BudgetGate::new(usd("5"));
        let clamped = BudgetGate::clamped(usd("5"), usd("20"));

        let cases = [
            // No ceiling at all: the ordinary case (§4), and the one where no
            // rate source was consulted because nothing needed one.
            (
                currency(),
                unlimited.ledger(),
                RateSource::None,
                r#"{"type":"metering_selected","turn":"00000000-0000-0000-0000-000000000002","metering":"currency","ceiling":{"effect":"none"},"rates":{"source":"none"}}"#,
            ),
            // A ceiling that binds, resting on the operator's own figures.
            (
                currency(),
                limited.ledger(),
                RateSource::Operator,
                r#"{"type":"metering_selected","turn":"00000000-0000-0000-0000-000000000002","metering":"currency","ceiling":{"effect":"binds","limit":{"amount":"5","currency":"USD"}},"rates":{"source":"operator"}}"#,
            ),
            // A currency row settled from the provider's own invoice. Its
            // ceiling still rests on rates, because a stated cost arrives
            // after the tokens are spent and can never bound them — and the
            // rates it rests on are the operator's, which §5 ranks first.
            (
                invoiced(),
                limited.ledger(),
                RateSource::Operator,
                r#"{"type":"metering_selected","turn":"00000000-0000-0000-0000-000000000002","metering":"currency","ceiling":{"effect":"binds","limit":{"amount":"5","currency":"USD"}},"rates":{"source":"operator"}}"#,
            ),
            // §5's third and weakest source: a ceiling resting on a third
            // party's summary of someone else's prices. This is the case §10
            // adds `rates` to the event *for* — the two lines above and this
            // one report the same binding ceiling and are materially
            // different guarantees, and a consumer that could not tell them
            // apart could not report honestly on either. The revision travels
            // with it because "a table said so" is not attributable until it
            // says which table.
            (
                currency(),
                limited.ledger(),
                RateSource::Table {
                    revision: PricingRevision("litellm@1.79.3#9f86d081".to_string()),
                },
                r#"{"type":"metering_selected","turn":"00000000-0000-0000-0000-000000000002","metering":"currency","ceiling":{"effect":"binds","limit":{"amount":"5","currency":"USD"}},"rates":{"source":"table","revision":"litellm@1.79.3#9f86d081"}}"#,
            ),
            // An administrator's non-relaxable ceiling reduced what was asked
            // for. §4 requires the clamp be stated, not silent.
            (
                currency(),
                clamped.ledger(),
                RateSource::Operator,
                r#"{"type":"metering_selected","turn":"00000000-0000-0000-0000-000000000002","metering":"currency","ceiling":{"effect":"binds","limit":{"amount":"5","currency":"USD"},"clamped_from":{"amount":"20","currency":"USD"}},"rates":{"source":"operator"}}"#,
            ),
            // The two arrangements no monetary ceiling can bind. The limit is
            // named anyway: an operator who passed `--budget` deserves to be
            // told it will never fire, not left to infer it.
            (
                Metering::Subscription {
                    revision: MeteringRevision("acme/pro-plan/2026-08".to_string()),
                },
                limited.ledger(),
                RateSource::None,
                r#"{"type":"metering_selected","turn":"00000000-0000-0000-0000-000000000002","metering":"subscription","ceiling":{"effect":"inapplicable","limit":{"amount":"5","currency":"USD"},"metering":"subscription"},"rates":{"source":"none"}}"#,
            ),
            (
                Metering::Unmetered {
                    revision: MeteringRevision("ego/builtin/local/v1".to_string()),
                },
                limited.ledger(),
                RateSource::None,
                r#"{"type":"metering_selected","turn":"00000000-0000-0000-0000-000000000002","metering":"unmetered","ceiling":{"effect":"inapplicable","limit":{"amount":"5","currency":"USD"},"metering":"unmetered"},"rates":{"source":"none"}}"#,
            ),
        ];

        for (metering, ledger, rates, expected) in cases {
            let event = PublicEvent::metering_selected(turn, &metering, ledger, &rates);
            assert_eq!(
                serde_json::to_string(&event).expect("it serialises"),
                expected,
                "for {metering:?} under {ledger:?} from {rates:?}"
            );
        }
    }

    /// `TurnStarted`, like `SessionStarted`, has no `DomainEvent`
    /// counterpart and is constructed directly by the session driver.
    #[test]
    fn turn_started_envelope() {
        let turn: TurnId = id(TURN_1);
        let event = PublicEvent::TurnStarted { turn };
        let actual = serde_json::to_string(&event).unwrap();
        assert_eq!(actual, include_str!("golden/turn_started.json").trim_end());
    }

    /// `apply`'s reducer tolerates a degenerate envelope with no item id
    /// (`state.rs`'s `item_mut` returns `None` rather than panicking).
    /// `project_public` must be equally total: an item-shaped event with
    /// nowhere to attach in the public per-item model is dropped, not
    /// panicked on.
    #[test]
    fn item_scoped_event_without_an_item_id_projects_to_none() {
        let turn: TurnId = id(TURN_1);
        let event = ev(
            Seq::default(),
            turn,
            None,
            EventKind::AssistantDelta {
                text: "orphaned".to_string(),
            },
        );
        assert_eq!(project_public(&event), None);
    }

    /// `UsageUpdated`'s `item` is optional independently of its content — a
    /// usage report tied to a specific generating item carries it, mirroring
    /// `Error`'s own optional `item`. The main projection golden only
    /// exercises the `None` case (a turn-level usage report), so this pins
    /// the `Some` case directly.
    #[test]
    fn usage_updated_carries_item_when_present() {
        let turn: TurnId = id(TURN_1);
        let item: ItemId = id(ITEM_ASSISTANT_1);
        let event = ev(
            Seq::default(),
            turn,
            Some(item),
            EventKind::UsageUpdated {
                usage: tokens(10, 0, 0, 5),
                cost_delta: priced("0.001"),
                attempt: None,
            },
        );
        assert_eq!(
            project_public(&event),
            Some(PublicEvent::UsageUpdated {
                turn,
                item: Some(item),
                usage_delta: tokens(10, 0, 0, 5),
                cost_delta: priced("0.001"),
            })
        );
    }

    /// The classification the adapter made is the one the consumer gets. A
    /// projection that re-derived it — sniffing "rate limited" out of the
    /// message, say — would be publishing a claim the wire never made, and
    /// the golden's `Transient` line cannot tell the two apart on its own
    /// because a sniffer would agree with it. So this asserts the disagreeing
    /// case: a message that reads transient, classified `Refused`, arrives
    /// `Refused`.
    #[test]
    fn an_error_is_published_with_the_class_the_adapter_gave_it() {
        let turn: TurnId = id(TURN_1);
        let event = ev(
            Seq::default(),
            turn,
            None,
            EventKind::Error {
                message: "rate limited, retrying".to_string(),
                kind: ErrorKind::Refused,
            },
        );
        assert_eq!(
            project_public(&event),
            Some(PublicEvent::Error {
                turn,
                item: None,
                message: "rate limited, retrying".to_string(),
                kind: ErrorKind::Refused,
            })
        );
    }

    /// `Unclassified` is published, not omitted. It is a statement — nobody
    /// classified this error — and a consumer that saw no `kind` at all could
    /// not tell it from a stream too old to carry one. The serialised form is
    /// asserted rather than the enum, because "always emitted" is a
    /// serialisation property and `skip_serializing_if` is one attribute away.
    #[test]
    fn an_unclassified_error_still_carries_its_class_on_the_wire() {
        let turn: TurnId = id(TURN_1);
        let event = ev(
            Seq::default(),
            turn,
            None,
            EventKind::Error {
                message: "something went wrong".to_string(),
                kind: ErrorKind::Unclassified,
            },
        );
        let json = serde_json::to_string(&project_public(&event).expect("errors are public"))
            .expect("a public event serialises");
        assert_eq!(
            json,
            r#"{"type":"error","turn":"00000000-0000-0000-0000-000000000002","message":"something went wrong","kind":"unclassified"}"#
        );
    }

    /// An unpriced generation must reach the public stream *as* unpriced.
    /// The failure this guards against is not a crash but a silent
    /// downgrade: if `cost_delta` were `Option<Money>`, the natural
    /// projection would be `None`, which every consumer would read as
    /// "free". The golden log only carries priced generations, so without
    /// this the `Unknown` path has no coverage at all.
    #[test]
    fn an_unpriced_generation_projects_as_unknown_never_as_free() {
        let turn: TurnId = id(TURN_1);
        let event = ev(
            Seq::default(),
            turn,
            None,
            EventKind::UsageUpdated {
                usage: tokens(10, 0, 0, 5),
                cost_delta: CostDelta::Unknown(UnknownCost::NoPricingQuote),
                attempt: None,
            },
        );
        assert_eq!(
            project_public(&event),
            Some(PublicEvent::UsageUpdated {
                turn,
                item: None,
                usage_delta: tokens(10, 0, 0, 5),
                cost_delta: CostDelta::Unknown(UnknownCost::NoPricingQuote),
            })
        );
    }

    /// Context occupancy is public, and carries its provenance. Without
    /// this, the arm could be deleted — the event would simply vanish from
    /// the stream — and every other test would stay green.
    #[test]
    fn context_window_updates_project_with_their_provenance() {
        let turn: TurnId = id(TURN_1);
        for provenance in [
            OccupancyProvenance::GenerationBoundary,
            OccupancyProvenance::AuthoritativeQuery,
            OccupancyProvenance::LocalEstimate,
        ] {
            let event = ev(
                Seq::default(),
                turn,
                None,
                EventKind::ContextWindowUpdated {
                    used_tokens: 4_096,
                    provenance,
                },
            );
            assert_eq!(
                project_public(&event),
                Some(PublicEvent::ContextWindowUpdated {
                    turn,
                    used_tokens: 4_096,
                    provenance,
                })
            );
        }
    }

    /// `ReasoningDelta` is the one `EventKind` the fixture log never
    /// contains, so without this test its projection has no runtime
    /// coverage: the golden would be equally green with the arm deleted or
    /// mapped to the wrong `ItemKind`.
    ///
    /// It is deliberately *not* `ItemKind::AgentMessage`: reasoning is a
    /// separate public item kind, so a client can render or suppress
    /// thinking independently of the answer.
    #[test]
    fn reasoning_delta_projects_to_an_item_delta_of_kind_reasoning() {
        let turn: TurnId = id(TURN_1);
        let item: ItemId = id(ITEM_ASSISTANT_1);
        let event = ev(
            Seq::default(),
            turn,
            Some(item),
            EventKind::ReasoningDelta {
                text: "weighing the options".to_string(),
            },
        );
        assert_eq!(
            project_public(&event),
            Some(PublicEvent::ItemDelta {
                turn,
                item,
                kind: ItemKind::Reasoning,
                text: "weighing the options".to_string(),
            })
        );
    }

    /// The three accounting kinds are not public, and that is a privacy
    /// boundary rather than an omission.
    ///
    /// `GenerationStarted`, `AttemptSettled` and `UsageCorrected` are the
    /// attempt ledger: which attempt of which generation was made, whether
    /// the request may have reached the provider, and what the provider later
    /// said it really cost. A consumer of the public stream is told the
    /// *totals* — `usage_delta` on the events that carry it — and never the
    /// bookings they were folded from, because an attempt id is an internal
    /// retry identity and a delivery certainty is a fact about our own
    /// transport.
    ///
    /// Pinned here because the golden log does not contain these kinds:
    /// without this, the projection could start emitting them and the only
    /// test that would notice is one nobody has written.
    #[test]
    fn accounting_kinds_are_not_public() {
        let generation = ego_core::event::id::GenerationId::new();
        let attempt = ego_core::generator::attempt::AttemptRef::first(generation);
        for kind in [
            EventKind::GenerationStarted {
                attempt,
                role: GenerationRole::Main,
            },
            EventKind::AttemptSettled {
                attempt,
                outcome: ego_core::generator::attempt::Outcome::new(
                    ego_core::generator::types::StopReason::Stop,
                    ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent,
                    ego_core::generator::attempt::UsageReport::Missing,
                ),
            },
            EventKind::UsageCorrected {
                attempt,
                usage: tokens(1, 0, 0, 1),
                cost_delta: priced("0.000018"),
            },
        ] {
            // With an item attached: the most favourable case for a wrong
            // mapping, since every arm that needs an id would have one.
            let name = kind.name();
            let event = ev(Seq::default(), id(TURN_1), Some(id(ITEM_ASSISTANT_1)), kind);
            assert_eq!(
                project_public(&event),
                None,
                "`{name}` reached the public stream, which carries totals and \
                 not the attempt ledger they came from"
            );
        }
    }

    /// `ItemKind` is a closed, snake_case-tagged set — pinned directly since
    /// no golden fixture line exercises every kind's string spelling on its
    /// own (some appear only via `item_started`/`item_completed`, never a
    /// bare serialisation).
    #[test]
    fn item_kind_serialises_snake_case() {
        // The two halves of what used to be one `message`. Pinned separately
        // because the whole point of the split is that a consumer can tell
        // them apart, and a single spelling test would not notice if both
        // arms serialised to the same string.
        assert_eq!(
            serde_json::to_string(&ItemKind::UserMessage).unwrap(),
            "\"user_message\""
        );
        assert_eq!(
            serde_json::to_string(&ItemKind::AgentMessage).unwrap(),
            "\"agent_message\""
        );
        assert_eq!(
            serde_json::to_string(&ItemKind::Reasoning).unwrap(),
            "\"reasoning\""
        );
        assert_eq!(serde_json::to_string(&ItemKind::Tool).unwrap(), "\"tool\"");
        assert_eq!(
            serde_json::to_string(&ItemKind::Permission).unwrap(),
            "\"permission\""
        );
        assert_eq!(
            serde_json::to_string(&ItemKind::Subagent).unwrap(),
            "\"subagent\""
        );
    }
}
