//! Process contracts for explicit unsupported-format cleanup (#1001-99d9).
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::session_lifecycle::SessionLifecycleStore;
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{JsonlStore, OsIo};
use std::process::{Command, Output};

fn run(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ego"))
        .env("EGO_HOME", root.join("home"))
        .args(args)
        .arg("--session-dir")
        .arg(root.join("sessions"))
        .current_dir(root)
        .output()
        .unwrap()
}

// Catches: cleanup stops at one unsupported log, removes a readable child or corrupt
// session, or bypasses explicit --yes. Exercises real processes and durable logs.
#[test]
fn rm_incompatible_removes_unreadable_sessions_and_keeps_readable_ones() {
    let root = tempfile::tempdir().unwrap();
    let sessions = root.path().join("sessions");
    let store = JsonlStore::new(&sessions, root.path(), Box::new(OsIo));
    let ids = [
        SessionId::new(),
        SessionId::new(),
        SessionId::new(),
        SessionId::new(),
    ];
    for id in ids {
        store
            .publish_empty_session(id, root.path(), Timestamp::EPOCH)
            .unwrap();
    }
    let mut seq = Seq::ZERO;
    store
        .append(
            &ids[2],
            &DomainEvent {
                env: Envelope {
                    seq: seq.next(),
                    id: EventId::new(),
                    turn: TurnId::new(),
                    item: None,
                    parent: None,
                    ts: Timestamp::EPOCH,
                    producer: Producer::Main,
                },
                kind: EventKind::Forked {
                    source: ids[0],
                    at_seq: Seq::ZERO,
                    snapshot: None,
                    neutral: false,
                },
            },
        )
        .unwrap();
    for id in &ids[..2] {
        let log = sessions.join(id.to_string()).join("events.jsonl");
        let bytes = std::fs::read_to_string(&log).unwrap();
        let (header, rest) = bytes.split_once('\n').unwrap();
        let mut header: serde_json::Value = serde_json::from_str(header).unwrap();
        header["v"] = 999.into();
        std::fs::write(log, format!("{header}\n{rest}")).unwrap();
    }
    // Corruption is not proof of an unsupported format and must not be deleted.
    std::fs::write(
        sessions.join(ids[3].to_string()).join("events.jsonl"),
        "broken\n",
    )
    .unwrap();
    let listed = run(root.path(), &["ls", "--all"]);
    assert!(listed.status.success(), "{listed:?}");
    let diagnostics = String::from_utf8_lossy(&listed.stderr);
    assert_eq!(
        diagnostics.matches("2 incompatible sessions").count(),
        1,
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains("ego rm --incompatible --yes"),
        "{diagnostics}"
    );
    assert!(!diagnostics.contains("unsupported log"), "{diagnostics}");
    let refused = run(root.path(), &["rm", "--incompatible"]);
    assert!(!refused.status.success());
    for id in ids {
        assert!(sessions.join(id.to_string()).exists());
    }
    let removed = run(root.path(), &["rm", "--incompatible", "--yes"]);
    assert!(removed.status.success(), "{removed:?}");
    for id in &ids[..2] {
        assert!(!sessions.join(id.to_string()).exists());
    }
    for id in &ids[2..] {
        assert!(sessions.join(id.to_string()).exists());
    }
    let report = String::from_utf8_lossy(&removed.stdout);
    assert!(report.contains(&format!("kept {}", ids[2])), "{report}");
    assert!(report.contains(&format!("parent {}", ids[0])), "{report}");
    assert!(
        run(root.path(), &["rm", "--incompatible", "--yes"])
            .status
            .success()
    );
    let mixed = run(
        root.path(),
        &["rm", &ids[2].to_string(), "--incompatible", "--yes"],
    );
    assert!(!mixed.status.success());
    assert!(sessions.join(ids[2].to_string()).exists());
}

// Catches: physical cleanup bypasses the writer lease or deletes a format that
// became readable after enumeration. A real held OS lease is the safety oracle.
#[test]
fn incompatible_cleanup_rechecks_format_and_refuses_active_writers() {
    use ego_tools::store::Lease;
    let root = tempfile::tempdir().unwrap();
    let store = JsonlStore::new(root.path(), root.path(), Box::new(OsIo));
    let id = SessionId::new();
    store
        .publish_empty_session(id, root.path(), Timestamp::EPOCH)
        .unwrap();
    assert!(!store.remove_incompatible(&id).unwrap());
    let dir = root.path().join(id.to_string());
    let lease = Lease::acquire(
        &dir.join("lease"),
        std::process::id(),
        Timestamp::EPOCH,
        std::time::Duration::from_secs(60),
        &|_| true,
    )
    .unwrap();
    let path = dir.join("events.jsonl");
    let bytes = std::fs::read_to_string(&path).unwrap();
    let (header, rest) = bytes.split_once('\n').unwrap();
    let mut header: serde_json::Value = serde_json::from_str(header).unwrap();
    header["v"] = 999.into();
    std::fs::write(path, format!("{header}\n{rest}")).unwrap();
    // Quarantine must not make unsupported sessions impossible to clean up.
    std::fs::write(dir.join("quarantined"), "hidden").unwrap();
    let mut incompatible = Vec::new();
    store
        .cleanup_inventory(&mut |found, _| incompatible.push(found))
        .unwrap();
    assert_eq!(incompatible, vec![id]);
    assert!(store.remove_incompatible(&id).is_err());
    assert!(dir.exists());
    drop(lease);
    assert!(store.remove_incompatible(&id).unwrap());
    assert!(!dir.exists());
}
