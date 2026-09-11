//! contract for `/name` non-interactive execution (regression case).

mod support;

use ego_core::event::id::SessionId;
use ego_core::event::time::Timestamp;
use ego_core::session_lifecycle::SessionLifecycleStore;
use ego_tools::store::{JsonlStore, LeaseInputs, OsIo};
use std::path::Path;
use std::time::Duration;
use support::Ego;

const ALIVE: &dyn Fn(u32) -> bool = &|_| true;

struct Fixture {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    sessions: tempfile::TempDir,
    id: String,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_path = std::fs::canonicalize(workspace.path()).unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        JsonlStore::new(sessions.path(), workspace.path(), Box::new(OsIo))
            .publish_empty_session(id, Path::new(&workspace_path), Timestamp::from_millis(1))
            .unwrap();
        Self {
            home,
            workspace,
            sessions,
            id: id.to_string(),
        }
    }

    fn run(&self, operation: &str, output: Option<&str>) -> support::Finished {
        self.run_for_id(&self.id, "--name", operation, output)
    }

    fn run_goal(&self, operation: &str, output: Option<&str>) -> support::Finished {
        self.run_for_id(&self.id, "--goal", operation, output)
    }

    fn run_for_id(
        &self,
        id: &str,
        flag: &str,
        operation: &str,
        output: Option<&str>,
    ) -> support::Finished {
        let workspace = std::fs::canonicalize(self.workspace.path()).unwrap();
        let mut command = Ego::at(self.home.path(), &workspace, None)
            .subcommand("resume")
            .arg(id)
            .arg("--session-dir")
            .arg(self.sessions.path().to_str().unwrap())
            .arg(flag)
            .arg(operation);
        if let Some(output) = output {
            command = command.arg("--output").arg(output);
        }
        command.run()
    }
}

#[test]
fn name_execution_matches_text_machine_and_failure_contracts() {
    let fixture = Fixture::new();
    let shown = fixture.run("show", None);
    assert_eq!(shown.code, Some(0));
    assert_eq!(shown.stdout, "name: (none)\n");
    let set = fixture.run("set=\u{2003}Release  Candidate\u{2003}", None);
    assert_eq!(set.code, Some(0));
    assert_eq!(set.stdout, "name: Release  Candidate\n");
    assert_eq!(
        fixture.run("show", None).stdout,
        "name: Release  Candidate\n"
    );
    assert_eq!(fixture.run("clear", None).stdout, "name: (none)\n");
    assert_eq!(
        fixture.run("show", Some("json")).report(),
        serde_json::json!({"v": 1, "session_id": fixture.id, "name": null})
    );
    assert_eq!(
        fixture.run("show", Some("jsonl")).stdout,
        "{\"v\":1,\"session_id\":\"".to_owned() + &fixture.id + "\",\"name\":null}\n"
    );
    for invalid in [
        "set=",
        "set=bad\ntext",
        &format!("set={}", "x".repeat(1025)),
    ] {
        assert_eq!(fixture.run(invalid, None).code, Some(2));
    }
    let unknown = SessionId::new().to_string();
    assert_eq!(
        fixture.run_for_id(&unknown, "--name", "show", None).code,
        Some(2)
    );
    let holder = JsonlStore::new(
        fixture.sessions.path(),
        fixture.workspace.path(),
        Box::new(OsIo),
    );
    let _lease = holder
        .open_writer(
            &fixture.id.parse().unwrap(),
            LeaseInputs {
                pid: 85_085,
                now: Timestamp::from_millis(85_085),
                stale_after: Duration::from_secs(30),
                is_alive: ALIVE,
            },
        )
        .unwrap();
    let conflict = fixture.run("set=held", None);
    assert_eq!(conflict.code, Some(1));
    assert_eq!(conflict.stdout, "name: (none)\n");
}

#[test]
fn goal_execution_snapshots_profile_ceiling_and_reaches_blocked_and_achieved() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.home.path().join("config.toml"),
        "[profile.audit]\nbudget = \"2k\"\n",
    )
    .unwrap();
    let before = fixture.run_goal("achieve", Some("json"));
    assert_eq!(before.code, Some(1));
    assert_eq!(before.report()["goal"], serde_json::Value::Null);
    let workspace = std::fs::canonicalize(fixture.workspace.path()).unwrap();
    let set = Ego::at(fixture.home.path(), &workspace, None)
        .env("EGO_PROFILE", "audit")
        .subcommand("resume")
        .arg(&fixture.id)
        .arg("--session-dir")
        .arg(fixture.sessions.path().to_str().unwrap())
        .arg("--goal")
        .arg("set=Ship the product")
        .arg("--output")
        .arg("json")
        .run();
    assert_eq!(set.code, Some(0));
    assert_eq!(set.report()["goal"]["ceilings"]["tokens"], 2000);
    assert!(set.report()["goal"].get("blocker_turns").is_none());
    let active_set = fixture.run_goal("set=another", Some("json"));
    assert_eq!(active_set.code, Some(1));
    for (operation, expected) in [
        ("block=API unavailable", "active"),
        ("block=API unavailable", "active"),
        ("block=API unavailable", "blocked"),
    ] {
        let result = fixture.run_goal(operation, Some("json"));
        assert_eq!(result.code, Some(0));
        assert_eq!(result.report()["goal"]["state"], expected);
    }
    fixture.run_goal("set=final", None);
    fixture.run_goal("achieve", None);
    let final_view = fixture.run_goal("show", Some("json"));
    assert_eq!(final_view.report()["goal"]["state"], "achieved");
    assert_eq!(final_view.report()["goal"]["achieved_by"], "operator");
    assert_eq!(
        final_view.report()["goal"]["achievement_evidence"],
        serde_json::Value::Null
    );
}
