//! regression case: every fork entry point carries the source title and active goal.

mod support;

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, GenerationRole};
use ego_core::event::id::{EventId, GenerationId, Producer, SessionId, TurnId};
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
