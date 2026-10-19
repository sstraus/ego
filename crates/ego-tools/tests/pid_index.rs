//! Host-visible lease index. Each test names the missing or stale binding it catches.

use std::fs;
use std::time::Duration;

use ego_core::event::id::SessionId;
use ego_core::event::time::Timestamp;
use ego_tools::store::{JsonlStore, LeaseInputs, OsIo};
use serde_json::{Value, json};

fn inputs() -> LeaseInputs<'static> {
    LeaseInputs {
        pid: 4242,
        now: Timestamp::from_millis(9000),
        stale_after: Duration::from_secs(60),
        is_alive: &|_| true,
    }
}

fn binding(home: &std::path::Path) -> Value {
    serde_json::from_slice(
        &fs::read(home.join("pids/4242.json")).expect("held lease has a PID index"),
    )
    .unwrap()
}

#[test]
fn per_pid_file_names_the_held_session() {
    // Catches an absent index or an index that does not identify the real lease.
    let home = tempfile::tempdir().unwrap();
    let cwd = fs::canonicalize(home.path()).unwrap();
    let store = JsonlStore::new(home.path().join("sessions"), &cwd, Box::new(OsIo))
        .with_pid_index(home.path().to_path_buf());
    let id = SessionId::new();
    let _opened = store.open_writer(&id, inputs()).unwrap();
    assert_eq!(
        binding(home.path()),
        json!({"pid": 4242, "session_id": id.to_string(), "cwd": cwd})
    );
    let lease: Value = serde_json::from_slice(
        &fs::read(
            home.path()
                .join("sessions")
                .join(id.to_string())
                .join("lease"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(lease["pid"], 4242);
    assert!(!home.path().join("pids/4242.json.tmp").exists());
}

#[test]
fn per_pid_file_is_removed_when_the_session_is_released() {
    // Catches a stale binding surviving release; evidence also owns the lease.
    let home = tempfile::tempdir().unwrap();
    let store = JsonlStore::new(home.path().join("sessions"), home.path(), Box::new(OsIo))
        .with_pid_index(home.path().to_path_buf());
    let id = SessionId::new();
    let opened = store.open_writer(&id, inputs()).unwrap();
    let evidence = opened.writer.evidence_store(1024).unwrap();
    assert_eq!(binding(home.path())["session_id"], id.to_string());
    drop(opened);
    assert!(home.path().join("pids/4242.json").exists());
    drop(evidence);
    assert!(!home.path().join("pids/4242.json").exists());
}

#[test]
fn per_pid_file_follows_a_session_switch() {
    // Catches the first session remaining indexed, or its release deleting the new binding.
    let home = tempfile::tempdir().unwrap();
    let store = JsonlStore::new(home.path().join("sessions"), home.path(), Box::new(OsIo))
        .with_pid_index(home.path().to_path_buf());
    let first = SessionId::new();
    let second = SessionId::new();
    let old = store.open_writer(&first, inputs()).unwrap();
    assert_eq!(binding(home.path())["session_id"], first.to_string());
    let new = store.open_writer(&second, inputs()).unwrap();
    assert_eq!(binding(home.path())["session_id"], second.to_string());
    drop(old);
    assert_eq!(binding(home.path())["session_id"], second.to_string());
    drop(new);
    assert!(!home.path().join("pids/4242.json").exists());
}
