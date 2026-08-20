//! `PublicEvent` projection: golden fixtures for the privacy-filtered,
//! versioned JSONL output contract.
//!
//! See `ARCHITECTURE.md` §1, `CLI.md` §4.2 and `plans/m0a-contracts.md`
//! Step 8. Fixtures live in `tests/golden/` as committed data files, not
//! inline string literals, so a change to the projection shows up as a
//! reviewable diff in a data file rather than a diff in test code.

use ego_core::cost::{Amount, CostDelta, CurrencyCode, Money, PricingRevision, UnknownCost};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason, OccupancyProvenance};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::public::{ItemKind, PUBLIC_EVENT_VERSION, PublicEvent, project_public};
use ego_core::state::Usage;

mod common;

// Wrapped in a module named `public_golden` so `cargo test -p ego-core
// public_golden` (the story's VERIFY command) matches these tests by
// substring, the same trick `tests/reducer.rs` uses for `reducer`: cargo's
// test filter matches on the full test path, not the source file name.
mod public_golden {
    use super::*;

    use super::common::id;

    const SESSION: &str = "00000000-0000-0000-0000-000000000001";

    /// Tokens across all four priced categories, so the golden pins every
    /// one of them rather than only the two the old input/output pair had.
    fn tokens(uncached_input: u64, cache_read: u64, cache_write: u64, output: u64) -> Usage {
        Usage {
            uncached_input_tokens: uncached_input,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            output_tokens: output,
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
            pricing_revision: PricingRevision("test-2026-08".to_string()),
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

    /// A realistic two-turn log of 21 events covering 17 of the 18
    /// `EventKind` variants: `UserPrompt`, `AssistantDelta`,
    /// `AssistantMessage` and `TurnCompleted` each appear more than once,
    /// and `ReasoningDelta` is absent — it is the one kind this log does
    /// not reach, so it is covered by
    /// `reasoning_delta_projects_to_an_item_delta_of_kind_reasoning` below
    /// instead. The first turn covers every public item kind
    /// (message, tool, permission, subagent), a `UsageUpdated` (public, as
    /// `usage_delta` — never a running total), a bare `Error`, and the
    /// three internal-only kinds (`Compacted`, `Forked`, `HookFired`) that
    /// must not appear in the projection. The second, final turn is
    /// deliberately minimal so the golden's last line is a plain
    /// `turn_completed` — proving "the final result rides the terminal
    /// event" needs no separate mechanism.
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
                    usage: tokens(120, 900, 60, 45),
                    cost_delta: priced("0.0027"),
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
                        | EventKind::HookFired { .. }
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            internal_kinds.len(),
            3,
            "fixture should exercise all three internal-only kinds"
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

    /// `SessionStarted` is the header event: `v:1`, a `capabilities` array,
    /// and `mcp_errors`/`hook_errors` omitted entirely (not emitted as
    /// `[]`) when there's nothing to report. It has no `DomainEvent`
    /// counterpart — `project_public` never returns it, the harness that
    /// drives a session constructs it directly — so this golden proves the
    /// DTO's own shape and serialisation rather than a projection.
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
        };
        let actual = serde_json::to_string(&event).unwrap();
        assert_eq!(
            actual,
            include_str!("golden/session_started_with_errors.json").trim_end()
        );
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
    /// It is deliberately *not* `ItemKind::Message`: reasoning is a
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

    /// `ItemKind` is a closed, snake_case-tagged set — pinned directly since
    /// no golden fixture line exercises every kind's string spelling on its
    /// own (some appear only via `item_started`/`item_completed`, never a
    /// bare serialisation).
    #[test]
    fn item_kind_serialises_snake_case() {
        assert_eq!(
            serde_json::to_string(&ItemKind::Message).unwrap(),
            "\"message\""
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
