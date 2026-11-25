#![cfg(windows)]

use std::fs;

use ego_core::permit::{CanonPath, FsError, PrepareCtx, ToolError};
use ego_tools::fs::OsFs;
use ego_tools::tools::builtins;
use serde_json::json;
use tempfile::tempdir;

/// Prepare `tool` in a fresh workspace holding only the file `kept`.
fn prepare(
    tool: &str,
    params: impl FnOnce(&str) -> serde_json::Value,
) -> (tempfile::TempDir, Result<(), ToolError>) {
    let dir = tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::write(root.join("kept"), "x").unwrap();
    let ctx = PrepareCtx {
        cwd: CanonPath::new(&root),
        resolver: &OsFs,
    };
    let registry = builtins();
    let tool = registry
        .iter()
        .find(|candidate| candidate.descriptor().name == tool)
        .expect("the tool is registered");
    let outcome = tool.prepare(params(root.to_str().unwrap()), &ctx).map(drop);
    (dir, outcome)
}

fn assert_stream_refusal(outcome: Result<(), ToolError>, spelled: &str) {
    match outcome {
        Err(ToolError::Fs(refusal @ FsError::StreamName { .. })) => {
            let shown = refusal.to_string();
            assert!(
                shown.contains("stream") && shown.contains(spelled),
                "{shown}"
            );
        }
        other => panic!("{spelled}: expected a stream refusal, got {other:?}"),
    }
}

// Catches: policy matching the stream text while NTFS opens the base file, so
// `.git::$DATA` creates the protected `.git` under a broad allow.
#[test]
fn a_stream_spelled_write_target_is_refused_before_the_file_system_is_touched() {
    for (spelled, base) in [
        (".git::$DATA", ".git"),
        ("a.exe:x.md", "a.exe"),
        ("sub\\new.exe:x.md", "sub\\new.exe"),
        ("kept:s", "kept"),
    ] {
        let (dir, outcome) = prepare(
            "write",
            |_| json!({"files": [{"path": spelled, "contents": "pwn"}]}),
        );
        assert_stream_refusal(outcome, spelled);
        let created = dir.path().join(base).exists();
        assert_eq!(created, base == "kept", "{base}");
    }
    // The absolute spelling is refused too.
    let (_dir, outcome) = prepare(
        "write",
        |root| json!({"files": [{"path": format!("{root}\\.git::$DATA"), "contents": "pwn"}]}),
    );
    assert!(matches!(
        outcome,
        Err(ToolError::Fs(FsError::StreamName { .. }))
    ));
}

// Catches: reads, listings and searches resolving a stream spelling that
// policy then judges by its text.
#[test]
fn a_stream_spelled_path_is_refused_for_read_list_and_search() {
    for (tool, params) in [
        ("read", json!({"files": [{"path": "kept:s"}]})),
        ("glob", json!({"pattern": "*", "path": "kept:s"})),
        ("grep", json!({"query": "x", "path": "kept:s"})),
    ] {
        let (_dir, outcome) = prepare(tool, |_| params);
        assert_stream_refusal(outcome, "kept:s");
    }
}
