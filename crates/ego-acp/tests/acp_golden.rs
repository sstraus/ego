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
//! This reuses the same 20-event, two-turn log as
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
//! the domain's 18 event kinds: its 21 events cover 17 distinct kinds
//! (`UserPrompt`, `AssistantDelta`, `AssistantMessage` and `TurnCompleted`
//! each appear more than once) and omit `ReasoningDelta` entirely.
//! Exhaustiveness is the job of the per-kind tests below —
//! `reasoning_delta_projects_to_a_thought_chunk_in_both_versions` covers
//! the kind the fixture omits — not of this log.
use agent_client_protocol_schema::{v1, v2};
use ego_acp::project::{project_acp_v1, project_acp_v2, project_v1, project_v2};
use ego_core::cost::{
    Amount, CostAttribution, CostDelta, CurrencyCode, Money, PricingRevision, UnknownCost,
};
use ego_core::event::domain::{
    CancelCause, DomainEvent, Envelope, EventKind, ExitReason, OccupancyProvenance,
};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::state::{SessionState, Usage};

// Wrapped in a module named `acp_golden` so `cargo test -p ego-acp
// acp_golden` (the story's VERIFY command) matches these tests by
// substring on the full test path — top-level functions in this file would
// not match, per `ego-core/tests/public_golden.rs`'s own note on the trap.
mod acp_golden {
    use super::*;

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

        let mut seq = Seq::default();
        vec![
            ev(
                seq.next(),
                turn1,
                Some(item_user_1),
                EventKind::UserPrompt {
                    text: "list the files".to_string(),
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
                    output: "file1\nfile2\n".to_string(),
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_perm_1),
                EventKind::PermissionRequested {
                    description: "write to /etc".to_string(),
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_perm_1),
                EventKind::PermissionDecided { allowed: false },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_sub_1),
                EventKind::SubagentSpawned {
                    subagent_id: "explorer-1".to_string(),
                },
            ),
            ev(
                seq.next(),
                turn1,
                Some(item_sub_1),
                EventKind::SubagentReturned {
                    subagent_id: "explorer-1".to_string(),
                    result: "done".to_string(),
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
                EventKind::HookFired {
                    name: "pre-tool-use".to_string(),
                    blocked: false,
                },
            ),
            ev(
                seq.next(),
                turn1,
                None,
                EventKind::Error {
                    message: "rate limited, retrying".to_string(),
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

    /// Every event kind in the domain's exhaustive 17-variant set, once.
    ///
    /// This is the *source* the `None`-set test selects from by name, so
    /// that a kind renamed or removed upstream breaks a test instead of
    /// silently shrinking the set being checked. It does not by itself
    /// prove every kind is covered — the test below filters it to the nine
    /// kinds it names, and the remaining eight are covered by the dedicated
    /// tests further down.
    fn one_of_each_kind() -> Vec<EventKind> {
        vec![
            EventKind::UserPrompt {
                text: String::new(),
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
                output: String::new(),
            },
            EventKind::PermissionRequested {
                description: String::new(),
            },
            EventKind::PermissionDecided { allowed: true },
            EventKind::UsageUpdated {
                usage: tokens(1, 1),
                cost_delta: priced("0.000018"),
                attempt: None,
            },
            EventKind::ContextWindowUpdated {
                used_tokens: 2,
                provenance: OccupancyProvenance::GenerationBoundary,
            },
            EventKind::TurnCompleted {
                ok: true,
                exit_reason: ExitReason::Completed,
            },
            EventKind::Compacted {
                summary: String::new(),
            },
            EventKind::Forked {
                source: SessionId::new(),
                at_seq: Seq::default(),
            },
            EventKind::HookFired {
                name: String::new(),
                blocked: false,
            },
            EventKind::SubagentSpawned {
                subagent_id: String::new(),
            },
            EventKind::SubagentReturned {
                subagent_id: String::new(),
                result: String::new(),
            },
            EventKind::Error {
                message: String::new(),
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
            schema_version: 1,
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
            .filter_map(project_v1)
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
            .filter_map(project_v2)
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
            },
        ] {
            // With an item attached: the most favourable case for a wrong
            // mapping, since every other arm that needs an id has one here.
            let event = with_item(kind);
            assert_eq!(project_v1(&event), None);
            assert_eq!(project_v2(&event), None);
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
        assert_eq!(project_v1(&event), None);
        assert_eq!(project_v2(&event), None);
    }

    /// Every domain event kind that has no `SessionUpdate` counterpart in
    /// *either* protocol version, matching `project.rs`'s module doc
    /// comment's "no counterpart in either version" list.
    #[test]
    fn events_with_no_counterpart_in_either_version_project_to_none() {
        let none_in_both = [
            "permission_requested",
            "permission_decided",
            "usage_updated",
            "compacted",
            "forked",
            "hook_fired",
            "subagent_spawned",
            "subagent_returned",
            "error",
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
                project_v1(&event),
                None,
                "{:?} must not project to a v1 SessionUpdate",
                event.kind
            );
            assert_eq!(
                project_v2(&event),
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
        assert_eq!(project_v1(&event), None);
        assert!(matches!(
            project_v2(&event),
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
            project_v1(&event),
            None,
            "v1 currently drops a non-streamed final message; if this now \
             emits an update, re-read project.rs's Known limitation section"
        );

        // And the same event under v2, to show the text is not
        // intrinsically unrepresentable — only unrepresentable statelessly
        // in v1.
        assert_eq!(
            project_v2(&event),
            Some(v2::SessionUpdate::AgentMessage(
                v2::AgentMessage::new(ITEM_ASSISTANT_2).content(vec!["You're welcome".into()])
            ))
        );

        // The v1 golden must not accidentally start containing it either:
        // the whole fixture log emits nothing carrying this text in v1.
        let v1_lines: Vec<String> = fixture_log()
            .iter()
            .filter_map(project_v1)
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
        assert_eq!(project_v1(&event), None);
        assert!(matches!(
            project_v2(&event),
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
            project_v1(&event),
            Some(v1::SessionUpdate::AgentThoughtChunk(
                v1::ContentChunk::new("weighing the options".into())
                    .message_id(Some(v1::MessageId::new(id)))
            ))
        );
        assert_eq!(
            project_v2(&event),
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
                assert_eq!(project_v1(&event), None, "{exit_reason:?} ok={ok}");
                assert_eq!(
                    project_v2(&event),
                    Some(v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
                        v2::IdleStateUpdate::new().stop_reason(expected_stop_reason.clone())
                    ))),
                    "{exit_reason:?} ok={ok}"
                );
            }
        }
    }

    /// v1's `ContentChunk.message_id` is optional, so **every** chunk-shaped
    /// event still projects without an item id — just without chunk
    /// continuity. Table-driven over all three rather than asserting it for
    /// `AssistantDelta` alone and generalising: the three arms are written
    /// separately in `project.rs`, so one of them regressing to `item.map`
    /// would go unnoticed by a single-variant test.
    ///
    /// `AssistantMessage` is deliberately absent: it projects to `None` in
    /// v1 with *or* without an item id (see
    /// `v1_loses_a_final_message_that_never_streamed`).
    #[test]
    fn every_v1_chunk_shaped_event_without_item_id_still_projects() {
        let cases = [
            (
                EventKind::UserPrompt {
                    text: "orphaned".to_string(),
                },
                v1::SessionUpdate::UserMessageChunk(v1::ContentChunk::new("orphaned".into())),
            ),
            (
                EventKind::AssistantDelta {
                    text: "orphaned".to_string(),
                },
                v1::SessionUpdate::AgentMessageChunk(v1::ContentChunk::new("orphaned".into())),
            ),
            (
                EventKind::ReasoningDelta {
                    text: "orphaned".to_string(),
                },
                v1::SessionUpdate::AgentThoughtChunk(v1::ContentChunk::new("orphaned".into())),
            ),
        ];
        for (kind, expected) in cases {
            let name = kind.name();
            let event = ev(Seq::default(), TurnId::new(), None, kind);
            assert_eq!(project_v1(&event), Some(expected), "{name}");
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
                output: "orphaned".to_string(),
            },
        ];
        for kind in kinds {
            let name = kind.name();
            let event = ev(Seq::default(), TurnId::new(), None, kind);
            assert_eq!(project_v2(&event), None, "{name}");
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
                output: "orphaned".to_string(),
            },
        ];
        for kind in kinds {
            let name = kind.name();
            let event = ev(Seq::default(), TurnId::new(), None, kind);
            assert_eq!(project_v1(&event), None, "v1 {name}");
            assert_eq!(project_v2(&event), None, "v2 {name}");
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

        let v1_update = project_acp_v1(last, &state, &caps).expect("used and size are both known");
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

        let v2_update = project_acp_v2(last, &state, &caps).expect("used and size are both known");
        let v2::SessionUpdate::UsageUpdate(v2_usage) = v2_update else {
            panic!("expected a v2 UsageUpdate, got {v2_update:?}");
        };
        assert_eq!((v2_usage.used, v2_usage.size), (165, 200_000));
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
        assert_eq!(project_acp_v1(last, &state, &caps), None);
        assert_eq!(project_acp_v2(last, &state, &caps), None);
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
        assert_eq!(project_acp_v1(&log[0], &state, &caps), None);
        assert_eq!(project_acp_v2(&log[0], &state, &caps), None);
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
                },
            ),
        ];
        let state = state_after(&log);
        let caps = caps(Some(200_000));

        let update = project_acp_v1(log.last().unwrap(), &state, &caps)
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
                project_acp_v1(event, &state, &caps),
                project_v1(event),
                "v1 {name}"
            );
            assert_eq!(
                project_acp_v2(event, &state, &caps),
                project_v2(event),
                "v2 {name}"
            );
        }
    }
}
