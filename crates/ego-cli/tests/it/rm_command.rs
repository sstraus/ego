//! Deletion crosses the real CLI and isolated durable store boundary.

use ego_core::event::id::SessionId;
use ego_core::event::time::Timestamp;
use ego_core::session::ReplaySource;
use ego_core::session_lifecycle::SessionLifecycleStore;
use ego_tools::store::{JsonlStore, OsIo};
use std::process::{Command, Stdio};

// Catches: wrong ID selection or storage removal deletes a neighbour's history.
#[test]
fn rm_only_hides_the_named_session_and_preserves_other_history() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(workspace.path()).unwrap();
    let store = JsonlStore::new(home.path().join("sessions"), root.clone(), Box::new(OsIo));
    let deleted = SessionId::new();
    let kept = SessionId::new();
    for id in [deleted, kept] {
        store
            .publish_empty_session(id, &root, Timestamp::from_millis(1))
            .unwrap();
    }
    let before = ReplaySource::entries(&store, &kept).unwrap().unwrap();
    let command = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ego"))
            .env("EGO_HOME", home.path())
            .current_dir(&root)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let refused = command(&["rm", &deleted.to_string()]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--yes"));
    let listed = command(&["ls", "--all", "--output", "json"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains(&deleted.to_string()));
    let removed = command(&["rm", &deleted.to_string(), "--yes"]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let listed = command(&["ls", "--all", "--output", "json"]);
    assert!(listed.status.success());
    let rows = String::from_utf8(listed.stdout).unwrap();
    assert!(!rows.contains(&deleted.to_string()), "{rows}");
    assert!(rows.contains(&kept.to_string()), "{rows}");
    assert_eq!(
        ReplaySource::entries(&store, &kept).unwrap().unwrap(),
        before
    );
    // This is a tombstone, not physical destruction of inherited history.
    assert!(ReplaySource::entries(&store, &deleted).unwrap().is_some());
}

// Catches: --force accidentally substitutes for confirmation or selects a nil ID.
#[test]
fn rm_parses_separate_force_and_confirmation_acknowledgements() {
    use ego_cli::cli::{Invocation, parse};
    let id = SessionId::new().to_string();
    let parsed = parse(vec!["ego".into(), "rm".into(), id, "--force".into()]).unwrap();
    let Invocation::Rm(args) = parsed else {
        panic!("not rm")
    };
    assert!(args.force);
    assert!(!args.yes);
    assert!(
        parse(vec![
            "ego".into(),
            "rm".into(),
            uuid::Uuid::nil().to_string()
        ])
        .is_err()
    );
}
