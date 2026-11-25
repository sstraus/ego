//! Startup failures must remain diagnosable after a host loses the terminal.

use crate::support::{Places, Plan, Reply, Stub, text};
use ego_core::event::id::SessionId;

// Catches: a real launch acquires its lease, then fails to open its append lock,
// leaving neither a journal nor a readable startup failure.
#[test]
fn failed_launch_after_lease_preserves_readable_startup_failure() {
    let places = Places::new();
    let id = SessionId::new().to_string();
    let directory = places.home_path().join("sessions").join(&id);
    std::fs::create_dir_all(directory.join("events.lock")).unwrap();
    let stub = Stub::start(Plan::default());
    let launch = || {
        places
            .ego(&stub)
            .args([
                "--sandbox",
                "off",
                "--yolo",
                "--trust-workspace",
                "--model",
                "ollama/qwen3",
                "--session-id",
                &id,
                "Hello",
            ])
            .run()
    };
    let result = launch();
    assert_ne!(result.code, Some(0), "{}", result.stdout);
    assert!(directory.join("lease").is_file(), "{}", result.stderr);
    assert!(!directory.join("manifest.json").exists());
    assert!(!directory.join("events.jsonl").exists());
    assert!(stub.generations().is_empty());
    let records: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("startup-")
        })
        .collect();
    assert_eq!(
        records.len(),
        1,
        "startup failure was lost: {}",
        result.stderr
    );
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&records[0]).unwrap()).unwrap();
    assert_eq!(record["session"], id);
    assert_eq!(record["outcome"], "failed");
    assert_eq!(record["exit_code"], result.code.unwrap());
    assert_eq!(record["phase"], "materialize inputs");
    let diagnostic = record["message"].as_str().unwrap();
    assert!(!diagnostic.is_empty());
    assert!(
        result.stderr.contains(diagnostic),
        "{record}\n{}",
        result.stderr
    );
    // Catches: retrying the same session erases the first launch's evidence.
    let first_bytes = std::fs::read(&records[0]).unwrap();
    let retry = launch();
    assert_ne!(retry.code, Some(0));
    assert_eq!(std::fs::read(&records[0]).unwrap(), first_bytes);
    assert_eq!(
        std::fs::read_dir(&directory)
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("startup-"))
            .count(),
        2
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&records[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

// Catches: the pending startup record remains after a real successful launch.
#[test]
fn successful_launch_removes_its_pending_startup_record() {
    let places = Places::new();
    let id = SessionId::new().to_string();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("ready"))],
        ..Plan::default()
    });
    let result = places
        .ego(&stub)
        .args([
            "--sandbox",
            "off",
            "--yolo",
            "--trust-workspace",
            "--model",
            "ollama/qwen3",
            "--session-id",
            &id,
            "Hello",
        ])
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert!(result.stdout.contains("ready"), "{}", result.stdout);
    let directory = places.home_path().join("sessions").join(id);
    assert!(directory.join("manifest.json").is_file());
    assert!(!std::fs::read_dir(directory).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("startup-")
    }));
}
