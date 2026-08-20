//! `PublicEvent` projection: golden fixtures for the privacy-filtered,
//! versioned JSONL output contract.
//!
//! See `ARCHITECTURE.md` §1, `CLI.md` §4.2 and `plans/m0a-contracts.md`
//! Step 8. Fixtures live in `tests/golden/` as committed data files, not
//! inline string literals, so a change to the projection shows up as a
//! reviewable diff in a data file rather than a diff in test code.

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::public::{ItemKind, PUBLIC_EVENT_VERSION, PublicEvent, project_public};
use ego_core::state::Usage;

// Wrapped in a module named `public_golden` so `cargo test -p ego-core
// public_golden` (the story's VERIFY command) matches these tests by
// substring, the same trick `tests/reducer.rs` uses for `reducer`: cargo's
// test filter matches on the full test path, not the source file name.
mod public_golden {
    use super::*;

    /// Deserialise a fixed UUID literal into any of the id newtypes, via
    /// their already-derived (and already-public) `Deserialize` impl. This
    /// gets deterministic, golden-comparable ids without adding a
    /// constructor to `event::id` (out of scope: owned by another story).
    fn id<T: serde::de::DeserializeOwned>(uuid: &str) -> T {
        serde_json::from_str(&format!("{uuid:?}")).unwrap()
    }

    const SESSION: &str = "00000000-0000-0000-0000-000000000001";
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

    /// A realistic two-turn log exercising every one of the 17 `EventKind`
    /// variants exactly once. The first turn covers every public item kind
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
                    input_tokens: 120,
                    output_tokens: 45,
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
                input_tokens: 10,
                output_tokens: 5,
            },
        );
        assert_eq!(
            project_public(&event),
            Some(PublicEvent::UsageUpdated {
                turn,
                item: Some(item),
                usage_delta: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
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
