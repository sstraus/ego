//! regression case: every fork entry point carries the source title and active goal.

mod support;

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, GenerationRole};
use ego_core::event::id::{EventId, GenerationId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::attempt::AttemptRef;
use ego_core::session::ReplaySource;
use ego_core::session_lifecycle::SessionLifecycleStore;
use ego_core::state::SessionState;
use ego_core::store::session_store::{SessionEntry, SessionStore};
use ego_tools::store::{JsonlStore, OsIo};
use serde_json::Value;
use std::path::Path;
use support::{Ego, MODEL, Plan, Reply, Stub};

const TITLE: &str = "Release Candidate";
const OBJECTIVE: &str = "Ship the product";

struct Fixture {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    sessions: tempfile::TempDir,
    source: String,
}

impl Fixture {
    /// A stored session with a title and an active goal, made through the
    /// public `resume --name/--goal` surface.
    fn titled_with_goal() -> Self {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(workspace.path()).unwrap();
        let id = SessionId::new();
        JsonlStore::new(sessions.path(), workspace.path(), Box::new(OsIo))
            .publish_empty_session(id, Path::new(&root), Timestamp::from_millis(1))
            .unwrap();
        let fixture = Self {
            home,
            workspace,
            sessions,
            source: id.to_string(),
        };
        assert_eq!(
            fixture
                .resume(&fixture.source, "--name", &format!("set={TITLE}"))
                .code,
            Some(0)
        );
        assert_eq!(
            fixture
                .resume(&fixture.source, "--goal", &format!("set={OBJECTIVE}"))
                .code,
            Some(0)
        );
        fixture
    }

    fn root(&self) -> std::path::PathBuf {
        std::fs::canonicalize(self.workspace.path()).unwrap()
    }

    fn resume(&self, id: &str, flag: &str, operation: &str) -> support::Finished {
        Ego::at(self.home.path(), &self.root(), None)
            .subcommand("resume")
            .arg(id)
            .arg("--session-dir")
            .arg(self.sessions.path().to_str().unwrap())
            .arg(flag)
            .arg(operation)
            .arg("--output")
            .arg("json")
            .run()
    }

    fn assert_carries_source_metadata(&self, child: &str) {
        let name = self.resume(child, "--name", "show").report();
        assert_eq!(name["name"], TITLE, "child title: {name}");
        let goal = self.resume(child, "--goal", "show").report();
        assert_eq!(goal["goal"]["objective"], OBJECTIVE, "child goal: {goal}");
        assert_eq!(goal["goal"]["state"], "active", "child goal: {goal}");
    }

    /// Append `GenerationStarted` with no settlement: the source now has a
    /// live attempt, which the fork policy must refuse.
    fn start_unsettled_attempt(&self) {
        let store = JsonlStore::new(self.sessions.path(), self.workspace.path(), Box::new(OsIo));
        let id: SessionId = self.source.parse().unwrap();
        let entries = store.entries(&id).unwrap().unwrap();
        let state = SessionState::replay(entries.into_iter().filter_map(|entry| match entry {
            SessionEntry::Event(event) => Some(event),
            _ => None,
        }));
        let mut seq = state.last_seq;
        seq.next();
        store
            .append(
                &id,
                &DomainEvent {
                    env: Envelope {
                        seq,
                        id: EventId::new(),
                        turn: TurnId::new(),
                        item: None,
                        parent: None,
                        ts: Timestamp::from_millis(5),
                        producer: Producer::Main,
                    },
                    kind: EventKind::GenerationStarted {
                        attempt: AttemptRef::first(GenerationId::new()),
                        role: GenerationRole::Main,
                    },
                },
            )
            .unwrap();
    }

    fn sessions_listing(&self) -> Vec<Value> {
        let listed = Ego::at(self.home.path(), &self.root(), None)
            .subcommand("ls")
            .arg("--session-dir")
            .arg(self.sessions.path().to_str().unwrap())
            .arg("--output")
            .arg("json")
            .run();
        assert_eq!(listed.code, Some(0), "ls failed: {}", listed.stderr);
        listed.report()["sessions"].as_array().unwrap().clone()
    }
}

#[test]
fn cli_fork_carries_the_source_title_and_goal() {
    let fixture = Fixture::titled_with_goal();
    let forked = Ego::at(fixture.home.path(), &fixture.root(), None)
        .subcommand("fork")
        .arg(&fixture.source)
        .arg("--session-dir")
        .arg(fixture.sessions.path().to_str().unwrap())
        .run();
    assert_eq!(forked.code, Some(0), "fork failed: {}", forked.stderr);
    fixture.assert_carries_source_metadata(forked.stdout.trim());
}

#[test]
fn interactive_fork_carries_the_source_title_and_goal() {
    let fixture = Fixture::titled_with_goal();
    let stub = Stub::start(Plan {
        installed: vec![MODEL.to_owned()],
        context_length: Some(32_768),
        completions: vec![Reply::Sse(support::text("unused"))],
        ..Plan::default()
    });
    let session = Ego::at(fixture.home.path(), &fixture.root(), Some(&stub))
        .subcommand("resume")
        .arg(&fixture.source)
        .args([
            "--session-dir",
            fixture.sessions.path().to_str().unwrap(),
            "--thinking",
            "off",
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
        ])
        .run_pty("/fork");
    assert_eq!(session.code, Some(0), "session failed: {}", session.stderr);
    let child = fixture
        .sessions_listing()
        .into_iter()
        .find(|row| row["lineage"]["kind"] == "fork")
        .expect("the interactive /fork published a child");
    fixture.assert_carries_source_metadata(child["id"].as_str().unwrap());
}

#[test]
fn cli_fork_refuses_a_source_with_a_live_attempt_and_publishes_nothing() {
    let fixture = Fixture::titled_with_goal();
    fixture.start_unsettled_attempt();
    let before = fixture.sessions_listing().len();
    let forked = Ego::at(fixture.home.path(), &fixture.root(), None)
        .subcommand("fork")
        .arg(&fixture.source)
        .arg("--session-dir")
        .arg(fixture.sessions.path().to_str().unwrap())
        .run();
    assert_ne!(forked.code, Some(0), "fork of a live source succeeded");
    assert!(
        forked.stderr.contains("live generation attempt"),
        "refusal names the live attempt: {}",
        forked.stderr
    );
    assert_eq!(
        fixture.sessions_listing().len(),
        before,
        "a target was published"
    );
}

impl Fixture {
    fn store(&self) -> JsonlStore {
        JsonlStore::new(self.sessions.path(), self.root(), Box::new(OsIo))
    }

    fn append_turn(&self, text: &str, ok: Option<bool>) -> (ItemId, ItemId, Seq) {
        let store = self.store();
        let source: SessionId = self.source.parse().unwrap();
        let state = SessionState::replay(
            store
                .entries(&source)
                .unwrap()
                .unwrap()
                .into_iter()
                .filter_map(|entry| match entry {
                    SessionEntry::Event(event) => Some(event),
                    _ => None,
                }),
        );
        let mut seq = state.last_seq;
        let user = ItemId::new();
        let assistant = ItemId::new();
        let turn = TurnId::new();
        let mut append = |item, kind| {
            let event = DomainEvent {
                env: Envelope {
                    seq: seq.next(),
                    id: EventId::new(),
                    turn,
                    item,
                    parent: None,
                    ts: Timestamp::from_millis(1),
                    producer: Producer::Main,
                },
                kind,
            };
            store.append(&source, &event).unwrap();
        };
        append(
            Some(user),
            EventKind::UserPrompt {
                text: text.into(),
                images: Vec::new(),
                queued_prompt: None,
            },
        );
        append(
            Some(assistant),
            EventKind::AssistantMessage {
                text: format!("answer {text}"),
            },
        );
        if let Some(ok) = ok {
            append(
                None,
                EventKind::TurnCompleted {
                    ok,
                    exit_reason: ego_core::event::domain::ExitReason::Completed,
                },
            );
        }
        (user, assistant, seq)
    }

    fn fork_at(&self, item: ItemId) -> support::Finished {
        Ego::at(self.home.path(), &self.root(), None)
            .subcommand("fork")
            .arg(&self.source)
            .arg("--at")
            .arg(&item.to_string())
            .arg("--session-dir")
            .arg(self.sessions.path().to_str().unwrap())
            .run()
    }

    fn replay_bytes(&self, child: SessionId) -> String {
        let store = self.store();
        serde_json::to_string(
            ego_core::session::ReplayContext::new(&store, &store)
                .replay(&child)
                .unwrap()
                .conversation(),
        )
        .unwrap()
    }
}

#[test]
fn cli_fork_at_turn_two_user_and_assistant_keeps_the_full_answer_and_excludes_turns_three_four() {
    let fixture = Fixture::titled_with_goal();
    fixture.append_turn("one", Some(true));
    let (user, assistant, cutoff) = fixture.append_turn("two", Some(true));
    fixture.append_turn("three", Some(true));
    fixture.append_turn("four", Some(true));
    for item in [user, assistant] {
        let result = fixture.fork_at(item);
        assert_eq!(result.code, Some(0), "{}", result.stderr);
        let child: SessionId = result.stdout.trim().parse().unwrap();
        assert_eq!(
            fixture.store().lineage(&child).unwrap(),
            Some(ego_core::session::SessionLineage::Fork {
                source: fixture.source.parse().unwrap(),
                at_seq: cutoff
            })
        );
        let bytes = fixture.replay_bytes(child);
        assert!(bytes.contains("answer two"));
        assert!(!bytes.contains("three"));
        assert!(!bytes.contains("four"));
    }
}

#[test]
fn cli_fork_at_live_source_ignores_the_writer_lease_and_concurrent_appends_do_not_tear_the_child() {
    let fixture = Fixture::titled_with_goal();
    let (_, item, _) = fixture.append_turn("settled", Some(true));
    fixture.start_unsettled_attempt();
    let store = fixture.store();
    let id: SessionId = fixture.source.parse().unwrap();
    let opened = store
        .open_writer(
            &id,
            ego_tools::store::LeaseInputs {
                pid: std::process::id(),
                now: Timestamp::from_millis(100),
                stale_after: std::time::Duration::from_secs(60),
                is_alive: &|_| true,
            },
        )
        .unwrap();
    let before = store.entries(&id).unwrap().unwrap();
    let attempt = SessionState::replay(before.iter().filter_map(|entry| match entry {
        SessionEntry::Event(event) => Some(event.clone()),
        _ => None,
    }))
    .generations
    .values()
    .next()
    .copied()
    .unwrap();
    std::thread::scope(|scope| {
        let append = scope.spawn(|| {
            // Append real journal records while the fork process reads it.
            let mut seq = opened.state.last_seq;
            for n in 0..32 {
                store
                    .append(
                        &id,
                        &DomainEvent {
                            env: Envelope {
                                seq: seq.next(),
                                id: EventId::new(),
                                turn: TurnId::new(),
                                item: None,
                                parent: None,
                                ts: Timestamp::from_millis(100),
                                producer: Producer::Main,
                            },
                            kind: EventKind::SessionTitleChanged {
                                title: Some(format!("later {n}")),
                            },
                        },
                    )
                    .unwrap();
            }
            store
                .append(
                    &id,
                    &DomainEvent {
                        env: Envelope {
                            seq: seq.next(),
                            id: EventId::new(),
                            turn: TurnId::new(),
                            item: None,
                            parent: None,
                            ts: Timestamp::from_millis(101),
                            producer: Producer::Main,
                        },
                        kind: EventKind::AttemptSettled {
                            attempt,
                            outcome: ego_core::generator::attempt::Outcome::new(
                                ego_core::generator::types::StopReason::Stop,
                                ego_core::generator::attempt::DeliveryCertainty::DefinitelySent,
                                ego_core::generator::attempt::UsageReport::Missing,
                            ),
                        },
                    },
                )
                .unwrap();
        });
        let result = fixture.fork_at(item);
        assert_eq!(result.code, Some(0), "{}", result.stderr);
        let child = result.stdout.trim().parse().unwrap();
        assert!(fixture.replay_bytes(child).contains("answer settled"));
        fixture.assert_carries_source_metadata(result.stdout.trim());
        append.join().unwrap();
    });
    let after = store.entries(&id).unwrap().unwrap();
    assert_eq!(
        &after[..before.len()],
        before.as_slice(),
        "fork changed source records"
    );
    let state = SessionState::replay(after.into_iter().filter_map(|entry| match entry {
        SessionEntry::Event(event) => Some(event),
        _ => None,
    }));
    assert!(
        state.live_attempts.is_empty(),
        "source settlement was not preserved"
    );
}

#[test]
fn interactive_fork_at_message_uses_the_same_cutoff_policy() {
    let fixture = Fixture::titled_with_goal();
    let (_, item, cutoff) = fixture.append_turn("one", Some(true));
    fixture.append_turn("two", Some(true));
    let stub = Stub::start(Plan {
        installed: vec![MODEL.to_owned()],
        context_length: Some(32_768),
        ..Plan::default()
    });
    let result = Ego::at(fixture.home.path(), &fixture.root(), Some(&stub))
        .subcommand("resume")
        .arg(&fixture.source)
        .args([
            "--session-dir",
            fixture.sessions.path().to_str().unwrap(),
            "--thinking",
            "off",
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
        ])
        .run_pty(&format!("/fork {item}"));
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    let child = fixture
        .sessions_listing()
        .into_iter()
        .find(|row| row["lineage"]["kind"] == "fork")
        .expect("/fork item published a child");
    let id = child["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        fixture.store().lineage(&id).unwrap(),
        Some(ego_core::session::SessionLineage::Fork {
            source: fixture.source.parse().unwrap(),
            at_seq: cutoff
        })
    );
    assert!(!fixture.replay_bytes(id).contains("answer two"));
}

#[test]
fn cli_fork_at_parent_compaction_cannot_change_child_conversation_bytes() {
    let fixture = Fixture::titled_with_goal();
    let (_, item, _) = fixture.append_turn("preserved", Some(true));
    let result = fixture.fork_at(item);
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    let child = result.stdout.trim().parse().unwrap();
    let before = fixture.replay_bytes(child);
    let checkpoint = "# Objective\nContinue.\n# Binding constraints and preferences\nPreserve authority.\n# Completed work and evidence\nOne turn.\n# Work in progress\nNone.\n# Decisions and rationale\nKeep the source.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nContinue.\n# Critical references\nSource journal.";
    let stub = Stub::start(Plan {
        installed: vec![MODEL.to_owned()],
        context_length: Some(32_768),
        completions: vec![Reply::Sse(support::text(checkpoint))],
        ..Plan::default()
    });
    let ego = Ego::at(fixture.home.path(), &fixture.root(), Some(&stub));
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    let compacted = ego
        .subcommand("compact")
        .arg(&fixture.source)
        .args([
            "--session-dir",
            fixture.sessions.path().to_str().unwrap(),
            "--output",
            "json",
        ])
        .run();
    assert_eq!(compacted.code, Some(0), "{}", compacted.stderr);
    assert_ne!(compacted.report()["target_session_id"], fixture.source);
    assert_eq!(fixture.replay_bytes(child), before);
}

#[test]
fn cli_fork_rejects_raw_sequences_and_foreign_items_without_tip_fallback() {
    let fixture = Fixture::titled_with_goal();
    fixture.append_turn("one", Some(true));
    let before = fixture.sessions_listing().len();
    for item in ["2".to_owned(), ItemId::new().to_string()] {
        let result = Ego::at(fixture.home.path(), &fixture.root(), None)
            .subcommand("fork")
            .arg(&fixture.source)
            .arg("--at")
            .arg(&item)
            .arg("--session-dir")
            .arg(fixture.sessions.path().to_str().unwrap())
            .run();
        assert_ne!(
            result.code,
            Some(0),
            "invalid item {item} became a tip fork"
        );
        assert_eq!(fixture.sessions_listing().len(), before);
    }
}
