//! `DomainEvent` and the pure sync reducer: replay determinism, totality
//! and causal-parent resolution.
//!
//! See `ARCHITECTURE.md` §1 and `plans/m0a-contracts.md` Step 2.

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::state::{PermissionEvent, SessionState, SubagentEvent};

// Wrapped in a module named `reducer` so `cargo test -p ego-core reducer`
// (the story's VERIFY command) matches these tests by substring: cargo's
// test filter matches on the full test path, not the source file name, and
// `tests/reducer.rs`'s bare functions wouldn't otherwise contain "reducer".
mod reducer {
    use super::*;

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

    /// A realistic single-turn log exercising every `EventKind` variant, with
    /// each event's `parent` pointing at the previous event's id.
    #[allow(unused_assignments)] // the final push!'s parent update is never re-read
    fn fixture_log() -> Vec<DomainEvent> {
        let turn = TurnId::new();
        let msg_item = ItemId::new();
        let reasoning_item = ItemId::new();
        let tool_item = ItemId::new();
        let source_session = SessionId::new();
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
                text: "list the files".to_string()
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
                args: "{\"pattern\":\"*\"}".to_string()
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
                output: "a.txt\nb.txt".to_string()
            }
        );
        push!(
            Some(tool_item),
            EventKind::PermissionRequested {
                description: "read workspace".to_string()
            }
        );
        push!(
            Some(tool_item),
            EventKind::PermissionDecided { allowed: true }
        );
        push!(
            None,
            EventKind::UsageUpdated {
                input_tokens: 42,
                output_tokens: 7
            }
        );
        push!(
            None,
            EventKind::HookFired {
                name: "pre-tool".to_string(),
                blocked: false
            }
        );
        push!(
            None,
            EventKind::SubagentSpawned {
                subagent_id: "sub-1".to_string()
            }
        );
        push!(
            None,
            EventKind::SubagentReturned {
                subagent_id: "sub-1".to_string(),
                result: "done".to_string()
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
                at_seq: Seq::default()
            }
        );
        push!(
            None,
            EventKind::Error {
                message: "a recoverable hiccup".to_string()
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
        assert_eq!(a.usage.input_tokens, 42);
        assert_eq!(a.usage.output_tokens, 7);
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
            },
        );
        let logically_first = ev(
            seq1,
            turn,
            None,
            None,
            EventKind::Error {
                message: "applied second".to_string(),
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
            },
        );
        let primer_id = primer.env.id;
        state.apply(&primer);
        let primed_seq = state.last_seq;

        let duplicate_id = EventId::new(); // reused as `id` on every event below
        let unknown_parent = EventId::new(); // never applied to this state
        let regressed_seq = Seq::default(); // ZERO — below `primed_seq`
        assert!(regressed_seq < primed_seq);

        let table: Vec<(&str, EventKind)> = vec![
            (
                "user_prompt",
                EventKind::UserPrompt {
                    text: String::new(),
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
                "tool_call_started",
                EventKind::ToolCallStarted {
                    name: String::new(),
                    args: String::new(),
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
                    output: String::new(),
                },
            ),
            (
                "permission_requested",
                EventKind::PermissionRequested {
                    description: String::new(),
                },
            ),
            (
                "permission_decided",
                EventKind::PermissionDecided { allowed: false },
            ),
            (
                "usage_updated",
                EventKind::UsageUpdated {
                    input_tokens: u64::MAX,
                    output_tokens: u64::MAX,
                },
            ),
            (
                "turn_completed",
                EventKind::TurnCompleted {
                    ok: false,
                    exit_reason: ExitReason::Error,
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
                },
            ),
            (
                "hook_fired",
                EventKind::HookFired {
                    name: String::new(),
                    blocked: false,
                },
            ),
            (
                "subagent_spawned",
                EventKind::SubagentSpawned {
                    subagent_id: String::new(),
                },
            ),
            (
                "subagent_returned",
                EventKind::SubagentReturned {
                    subagent_id: String::new(),
                    result: String::new(),
                },
            ),
            (
                "error",
                EventKind::Error {
                    message: String::new(),
                },
            ),
        ];
        assert_eq!(
            table.len(),
            17,
            "keep this table in sync with EventKind's variant count (ARCH §1)"
        );

        for (name, kind) in table {
            assert_eq!(
                kind.name(),
                name,
                "table entry out of sync with EventKind::name()"
            );
            let e = DomainEvent {
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
            state.apply(&e); // must not panic, for every variant, under every degenerate axis at once
        }

        assert_eq!(
            state.unresolved_parents.len(),
            17,
            "every event's parent was unresolvable"
        );
        assert_eq!(
            state.last_seq, regressed_seq,
            "apply must not clamp/reject the regressed seq"
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
        assert_eq!(state.usage.input_tokens, u64::MAX);
        assert_eq!(state.usage.output_tokens, u64::MAX);
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
                input_tokens: u64::MAX - 1,
                output_tokens: u64::MAX - 1,
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
                input_tokens: 10,
                output_tokens: 10,
            },
        ));

        assert_eq!(state.usage.input_tokens, u64::MAX);
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
                output: "final output".to_string(),
            },
        ));

        let it = &state.items[&item];
        assert_eq!(it.tool_name.as_deref(), Some("grep"));
        assert_eq!(it.text, "final output");
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
                description: "write file".to_string(),
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            Some(item),
            None,
            EventKind::PermissionDecided { allowed: true },
        ));

        assert_eq!(
            state.permissions,
            vec![
                PermissionEvent::Requested {
                    item: Some(item),
                    description: "write file".to_string()
                },
                PermissionEvent::Decided {
                    item: Some(item),
                    allowed: true
                },
            ]
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
                input_tokens: 10,
                output_tokens: 2,
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::UsageUpdated {
                input_tokens: 5,
                output_tokens: 1,
            },
        ));
        assert_eq!(state.usage.input_tokens, 15);
        assert_eq!(state.usage.output_tokens, 3);
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

        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::SubagentSpawned {
                subagent_id: "s1".to_string(),
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::SubagentReturned {
                subagent_id: "s1".to_string(),
                result: "ok".to_string(),
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::HookFired {
                name: "pre-commit".to_string(),
                blocked: true,
            },
        ));
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
            },
        ));
        state.apply(&ev(
            seq.next(),
            turn,
            None,
            None,
            EventKind::Error {
                message: "oops".to_string(),
            },
        ));

        assert_eq!(
            state.subagents,
            vec![
                SubagentEvent::Spawned {
                    subagent_id: "s1".to_string()
                },
                SubagentEvent::Returned {
                    subagent_id: "s1".to_string(),
                    result: "ok".to_string()
                },
            ]
        );
        assert_eq!(state.hooks.len(), 1);
        assert!(state.hooks[0].blocked);
        assert_eq!(state.compactions, vec!["trimmed".to_string()]);
        assert_eq!(state.forks.len(), 1);
        assert_eq!(state.forks[0].source, source);
        assert_eq!(state.errors, vec!["oops".to_string()]);
    }
}
