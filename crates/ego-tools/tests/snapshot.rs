//! Snapshots: bounded replay, and the cases where a snapshot must be
//! thrown away rather than trusted.
//!
//! See `plans/m0a-contracts.md` Step 7 and `ARCHITECTURE.md` §5.
//!
//! The load-bearing assertion in this file is *state equality*:
//! `resume(snapshot + suffix)` must equal `resume(full log)`. It is worth
//! being precise about why that is a strong test rather than a tautology.
//! `SessionState` derives `PartialEq` over every field it has — turns,
//! items, the `known_events` set, `unresolved_parents`, usage totals,
//! permissions, hooks, subagents, errors, compactions, forks — so a
//! snapshot path that dropped, duplicated or reordered a single event
//! changes at least one of them. Nothing here compares a summary or a
//! digest that could agree by luck.

use std::num::NonZeroUsize;

use ego_core::cost::{Amount, CostDelta, CumulativeCost, CurrencyCode, Money, PricingRevision};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason, OccupancyProvenance};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::state::{SessionState, Usage};
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{
    JsonlStore, OsIo, SNAPSHOT_VERSION, Snapshot, SnapshotPolicy, SnapshotRejection, validate,
};
use tempfile::{TempDir, tempdir};

// Wrapped in a module named `snapshot` so `cargo test -p ego-tools
// snapshot` (the story's VERIFY command) matches these tests by substring:
// cargo's filter matches the full test path, not the source file name.
mod snapshot {
    use super::*;

    struct Fixture {
        _dir: TempDir,
        store: JsonlStore,
        id: SessionId,
        events: Vec<DomainEvent>,
    }

    impl Fixture {
        fn snapshot_path(&self) -> std::path::PathBuf {
            self._dir
                .path()
                .join(session_dir_name(&self.id))
                .join("snapshot.json")
        }

        /// The reduction of the whole log, computed independently of the
        /// store's own resume path — this is the oracle every bounded
        /// resume is compared against.
        fn full_replay(&self) -> SessionState {
            let mut state = SessionState::default();
            for ev in &self.events {
                state.apply(ev);
            }
            state
        }

        /// Reduce the first `n` events, as a snapshot taken mid-log would
        /// have.
        fn replay_prefix(&self, n: usize) -> (Seq, SessionState) {
            let mut state = SessionState::default();
            for ev in self.events.iter().take(n) {
                state.apply(ev);
            }
            (self.events[n - 1].env.seq, state)
        }
    }

    fn tokens(uncached_input: u64, output: u64) -> Usage {
        Usage {
            uncached_input_tokens: uncached_input,
            output_tokens: output,
            ..Usage::default()
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

    fn session_dir_name(id: &SessionId) -> String {
        match serde_json::to_value(id).expect("SessionId always serialises") {
            serde_json::Value::String(s) => s,
            other => panic!("SessionId did not serialise as a JSON string: {other:?}"),
        }
    }

    /// A log deliberately rich enough that state equality means something:
    /// it touches turns, items, usage, permissions, hooks, subagents,
    /// errors and compactions, and it carries a causal `parent` so
    /// `known_events` and `unresolved_parents` are exercised too.
    fn session_with_events() -> Fixture {
        let dir = tempdir().unwrap();
        let store = JsonlStore::new(dir.path(), Box::new(OsIo));
        let id = SessionId::new();
        let turn1 = TurnId::new();
        let turn2 = TurnId::new();
        let item_msg = ItemId::new();
        let item_tool = ItemId::new();
        let mut seq = Seq::default();

        let first_id = EventId::new();
        let mut events = Vec::new();
        let mut push = |seq: Seq, turn, item, kind, parent, ev_id| {
            events.push(DomainEvent {
                env: Envelope {
                    seq,
                    id: ev_id,
                    turn,
                    item,
                    parent,
                    ts: Timestamp::from_millis(1_000 + seq_index(seq)),
                    producer: Producer::Main,
                },
                kind,
            });
        };

        push(
            seq.next(),
            turn1,
            Some(item_msg),
            EventKind::UserPrompt {
                text: "list the files".to_string(),
            },
            None,
            first_id,
        );
        push(
            seq.next(),
            turn1,
            Some(item_msg),
            EventKind::AssistantDelta {
                text: "Sure".to_string(),
            },
            Some(first_id),
            EventId::new(),
        );
        push(
            seq.next(),
            turn1,
            Some(item_tool),
            EventKind::ToolCallStarted {
                name: "bash".to_string(),
                args: "ls".to_string(),
                call_id: Some("call_a".to_string()),
            },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn1,
            Some(item_tool),
            EventKind::ToolCallCompleted {
                ok: true,
                output: "file1\n".to_string(),
            },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn1,
            None,
            EventKind::UsageUpdated {
                usage: tokens(120, 45),
                cost_delta: priced("0.001035"),
                attempt: None,
            },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn1,
            None,
            EventKind::ContextWindowUpdated {
                used_tokens: 165,
                provenance: OccupancyProvenance::GenerationBoundary,
            },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn1,
            None,
            EventKind::HookFired {
                name: "pre-tool-use".to_string(),
                blocked: false,
            },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn1,
            None,
            EventKind::TurnCompleted {
                ok: true,
                exit_reason: ExitReason::Completed,
            },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn1,
            Some(item_tool),
            EventKind::PermissionRequested {
                description: "write to /etc".to_string(),
            },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn1,
            Some(item_tool),
            EventKind::PermissionDecided { allowed: false },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn1,
            None,
            EventKind::SubagentSpawned {
                subagent_id: "explorer-1".to_string(),
            },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn1,
            None,
            EventKind::SubagentReturned {
                subagent_id: "explorer-1".to_string(),
                result: "done".to_string(),
            },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn2,
            None,
            EventKind::Compacted {
                summary: "earlier turns summarised".to_string(),
            },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn2,
            None,
            EventKind::Error {
                message: "rate limited".to_string(),
            },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn2,
            None,
            EventKind::UsageUpdated {
                usage: tokens(10, 5),
                cost_delta: priced("0.000105"),
                attempt: None,
            },
            None,
            EventId::new(),
        );
        push(
            seq.next(),
            turn2,
            None,
            EventKind::TurnCompleted {
                ok: true,
                exit_reason: ExitReason::Completed,
            },
            None,
            EventId::new(),
        );

        for ev in &events {
            store.append(&id, ev).unwrap();
        }

        Fixture {
            _dir: dir,
            store,
            id,
            events,
        }
    }

    /// `Seq`'s inner value, read back through its `Serialize` impl — it is
    /// a newtype with no accessor, exactly like `SessionId`.
    fn seq_index(seq: Seq) -> u64 {
        serde_json::to_value(seq).unwrap().as_u64().unwrap()
    }

    // -- AC1: resume(snapshot + suffix) == resume(full log) ---------------

    /// The central property. A snapshot taken at every possible point in
    /// the log must produce exactly the same state as replaying the whole
    /// thing — not approximately, and not only at convenient boundaries.
    #[test]
    fn snapshot_plus_suffix_equals_full_replay_at_every_cut_point() {
        let fx = session_with_events();
        let expected = fx.full_replay();

        // Sanity: the oracle must be non-trivial, or equality proves
        // nothing.
        assert!(!expected.turns.is_empty());
        assert!(!expected.items.is_empty());
        assert_ne!(expected.usage.uncached_input_tokens, 0);
        assert_eq!(
            expected.cost,
            CumulativeCost::Known {
                total: usd("0.00114")
            },
            "the two priced generations must have summed exactly"
        );
        assert_eq!(expected.context.used_tokens, Some(165));
        assert!(!expected.permissions.is_empty());
        assert!(!expected.subagents.is_empty());
        assert!(!expected.hooks.is_empty());
        assert!(!expected.errors.is_empty());
        assert!(!expected.compactions.is_empty());
        assert!(!expected.known_events.is_empty());

        for n in 1..=fx.events.len() {
            let (source_seq, prefix_state) = fx.replay_prefix(n);
            fx.store
                .write_snapshot(&fx.id, source_seq, &prefix_state)
                .unwrap();

            let (resumed, rejection) = fx.store.resume(&fx.id).unwrap();

            assert_eq!(rejection, None, "cut {n}: snapshot should have been used");
            assert_eq!(resumed, expected, "cut {n}: bounded resume diverged");
        }
    }

    /// With no snapshot at all, resume must still produce the same state —
    /// by replaying — and must say that it did.
    #[test]
    fn snapshot_absent_falls_back_to_full_replay() {
        let fx = session_with_events();

        let (resumed, rejection) = fx.store.resume(&fx.id).unwrap();

        assert_eq!(rejection, Some(SnapshotRejection::Absent));
        assert_eq!(resumed, fx.full_replay());
    }

    /// A snapshot records the `seq` it was taken at, and round-trips
    /// through the file unchanged.
    #[test]
    fn snapshot_records_its_source_seq_and_round_trips() {
        let fx = session_with_events();
        let (source_seq, prefix_state) = fx.replay_prefix(4);

        fx.store
            .write_snapshot(&fx.id, source_seq, &prefix_state)
            .unwrap();

        let written: Snapshot =
            serde_json::from_slice(&std::fs::read(fx.snapshot_path()).unwrap()).unwrap();
        assert_eq!(written.v, SNAPSHOT_VERSION);
        assert_eq!(written.session, fx.id);
        assert_eq!(written.source_seq, source_seq);
        assert_eq!(written.state, prefix_state);
    }

    // -- AC1: stale snapshots are rejected --------------------------------

    /// **The staleness case that actually happens.** regression case's tail
    /// repair truncates a torn log, so a snapshot can legitimately end up
    /// *ahead* of the log it describes. Trusting it would resurrect events
    /// the repair deliberately discarded.
    #[test]
    fn snapshot_ahead_of_a_repaired_log_is_rejected_and_does_not_resurrect_events() {
        let fx = session_with_events();

        // Snapshot the whole session, then damage and repair the log so it
        // ends earlier than the snapshot does.
        let (source_seq, full_state) = fx.replay_prefix(fx.events.len());
        fx.store
            .write_snapshot(&fx.id, source_seq, &full_state)
            .unwrap();

        let log_path = fx
            ._dir
            .path()
            .join(session_dir_name(&fx.id))
            .join("events.jsonl");
        let bytes = std::fs::read(&log_path).unwrap();
        let keep = bytes
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'\n')
            .map(|(i, _)| i + 1)
            .nth(5)
            .unwrap();
        std::fs::write(&log_path, &bytes[..keep]).unwrap();
        fx.store.recover(&fx.id).unwrap();

        let (resumed, rejection) = fx.store.resume(&fx.id).unwrap();

        assert!(
            matches!(rejection, Some(SnapshotRejection::AheadOfLog { .. })),
            "expected AheadOfLog, got {rejection:?}"
        );

        // The decisive assertion: the resumed state matches the *repaired*
        // log, not the snapshot. If the snapshot had been trusted, the
        // truncated events would be back.
        let mut expected = SessionState::default();
        for ev in fx.events.iter().take(5) {
            expected.apply(ev);
        }
        assert_eq!(resumed, expected);
        assert_ne!(resumed, full_state, "truncated events must not reappear");
    }

    /// A snapshot belonging to another session must never be applied to
    /// this one's log, however well-formed it is.
    #[test]
    fn snapshot_from_a_different_session_is_rejected() {
        let fx = session_with_events();
        let (source_seq, prefix_state) = fx.replay_prefix(4);
        let foreign = Snapshot::new(SessionId::new(), source_seq, prefix_state);
        std::fs::write(fx.snapshot_path(), serde_json::to_vec(&foreign).unwrap()).unwrap();

        let (resumed, rejection) = fx.store.resume(&fx.id).unwrap();

        assert_eq!(rejection, Some(SnapshotRejection::WrongSession));
        assert_eq!(resumed, fx.full_replay());
    }

    /// A snapshot written by a different format version is ignored rather
    /// than parsed optimistically.
    #[test]
    fn snapshot_with_an_unknown_version_is_rejected() {
        let fx = session_with_events();
        let (source_seq, prefix_state) = fx.replay_prefix(4);
        let mut future = Snapshot::new(fx.id, source_seq, prefix_state);
        future.v = SNAPSHOT_VERSION + 1;
        std::fs::write(fx.snapshot_path(), serde_json::to_vec(&future).unwrap()).unwrap();

        let (resumed, rejection) = fx.store.resume(&fx.id).unwrap();

        assert_eq!(
            rejection,
            Some(SnapshotRejection::Version {
                found: SNAPSHOT_VERSION + 1
            })
        );
        assert_eq!(resumed, fx.full_replay());
    }

    /// A snapshot a *previous build of this binary* wrote, byte for byte, not
    /// a synthetic one with its version field poked. `ItemState` gained four
    /// required fields in
    /// `docs/decisions/2026-08-21-tool-use-history.md`, so a v2 snapshot no
    /// longer even parses — and the point of this test is that it does not
    /// have to. Whichever rejection fires, resume produces the same state a
    /// full replay does, which is what makes "a snapshot is a cache" true
    /// rather than aspirational.
    #[test]
    fn a_snapshot_from_before_the_item_fields_falls_back_to_a_full_replay() {
        let fx = session_with_events();
        let (source_seq, _) = fx.replay_prefix(4);
        let v2 = serde_json::json!({
            "v": 2,
            "session": fx.id,
            "source_seq": source_seq,
            "state": {
                "last_seq": source_seq,
                "turns": {},
                // The shape v2 wrote: no seq, no kind, no call_id, no args,
                // and a `text` field carrying whichever of the two a tool
                // call happened to have last.
                "items": {},
                "known_events": [],
                "unresolved_parents": [],
                "usage": {
                    "uncached_input_tokens": 0,
                    "cache_read_tokens": 0,
                    "cache_write_tokens": 0,
                    "output_tokens": 0
                },
                "cost": { "Known": [] },
                "charges": [],
                "settled_attempts": {},
                "live_attempts": {},
                "attribution_began": false,
                "refusals": [],
                "context": { "used_tokens": null, "provenance": null },
                "permissions": [],
                "hooks": [],
                "subagents": [],
                "errors": [],
                "compactions": [],
                "forks": []
            }
        });
        std::fs::write(fx.snapshot_path(), serde_json::to_vec(&v2).unwrap()).unwrap();

        let (resumed, rejection) = fx.store.resume(&fx.id).unwrap();

        assert!(
            rejection.is_some(),
            "a v2 snapshot is never silently accepted"
        );
        assert_eq!(resumed, fx.full_replay());
    }

    /// A corrupt snapshot file degrades to a full replay — it is a cache,
    /// and losing a cache must never lose a session.
    #[test]
    fn snapshot_that_does_not_parse_degrades_to_full_replay() {
        let fx = session_with_events();
        std::fs::write(fx.snapshot_path(), b"{not json").unwrap();

        let (resumed, rejection) = fx.store.resume(&fx.id).unwrap();

        assert!(
            matches!(rejection, Some(SnapshotRejection::Unreadable(_))),
            "expected Unreadable, got {rejection:?}"
        );
        assert_eq!(resumed, fx.full_replay());
    }

    /// `validate` is the whole trust decision as a pure function, so the
    /// gap case can be stated directly rather than manufactured on disk.
    #[test]
    fn snapshot_validate_accepts_an_adjacent_log_and_rejects_a_gapped_one() {
        let id = SessionId::new();
        let mut seq = Seq::default();
        let (s3, s4, s5, s6) = (seq.next(), seq.next(), seq.next(), seq.next());
        let snapshot = Snapshot::new(id, s4, SessionState::default());

        // A log trimmed to exactly the snapshot's suffix is adjacent, not
        // gapped: nothing is missing between them.
        assert_eq!(validate(&snapshot, &id, s5, s6), Ok(()));

        // A log that still contains the snapshotted events is fine too.
        assert_eq!(validate(&snapshot, &id, s3, s6), Ok(()));

        // A log starting past the adjacency point leaves a hole.
        assert_eq!(
            validate(&snapshot, &id, s6, s6),
            Err(SnapshotRejection::Gap {
                snapshot: s4,
                next_in_log: s6
            })
        );
    }

    // -- AC2: configurable event-count interval ---------------------------

    /// The interval is a count of applied events, and is configurable.
    #[test]
    fn snapshot_policy_fires_on_the_configured_event_count() {
        let policy = SnapshotPolicy::every(NonZeroUsize::new(3).unwrap());

        assert_eq!(policy.interval(), 3);
        assert!(!policy.is_due(0));
        assert!(!policy.is_due(2));
        assert!(policy.is_due(3));
        // Past the interval it stays due: a caller that missed the exact
        // tick must still snapshot rather than wait a whole interval more.
        assert!(policy.is_due(4));

        let rarer = SnapshotPolicy::every(NonZeroUsize::new(500).unwrap());
        assert!(!rarer.is_due(3));
        assert_eq!(rarer.interval(), 500);
    }

    /// Driving the policy over a log must snapshot at the configured
    /// cadence, and each snapshot it writes must still resume correctly —
    /// the interval must not be able to produce a snapshot that is right
    /// about *when* and wrong about *what*.
    #[test]
    fn snapshot_policy_driven_over_a_log_writes_usable_snapshots() {
        let fx = session_with_events();
        let policy = SnapshotPolicy::every(NonZeroUsize::new(3).unwrap());
        let expected = fx.full_replay();

        let mut state = SessionState::default();
        let mut since_last = 0usize;
        let mut written = 0usize;

        for ev in &fx.events {
            state.apply(ev);
            since_last += 1;
            if policy.is_due(since_last) {
                fx.store.write_snapshot(&fx.id, ev.env.seq, &state).unwrap();
                since_last = 0;
                written += 1;

                let (resumed, rejection) = fx.store.resume(&fx.id).unwrap();
                assert_eq!(rejection, None);
                assert_eq!(
                    resumed, expected,
                    "snapshot {written} did not resume cleanly"
                );
            }
        }

        assert_eq!(
            written,
            fx.events.len() / 3,
            "11 events, every 3 => 3 snapshots"
        );
    }
}
