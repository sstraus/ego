use super::*;

// Catches: later files or original-text chunks are skipped or rematched after replacement.
#[test]
fn multifile_edit_applies_original_chunks_to_each_file() {
    let ws = Workspace::in_mode(Mode::Edits);
    let first = ws.file("src/first.txt", "alpha\r\nbeta\r\nend");
    let second = ws.file("src/second.txt", "one\ntwo\n");
    let result = ws
        .call(
            "edit",
            json!({"files": [
                {"path": "src/first.txt", "chunks": [
                    {"old_string": "alpha", "new_string": "beta"},
                    {"old_string": "beta", "new_string": "γ"}
                ]},
                {"path": "src/second.txt", "chunks": [
                    {"old_string": "one", "new_string": "ONE"},
                    {"old_string": "two", "new_string": "TWO"}
                ]}
            ]}),
        )
        .expect("multi-file edit");
    assert_eq!(fs::read(first).unwrap(), "beta\r\nγ\r\nend".as_bytes());
    assert_eq!(fs::read(second).unwrap(), b"ONE\nTWO\n");
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["files"][0]["path"], "src/first.txt");
    assert_eq!(result["files"][0]["status"], "completed");
    assert_eq!(result["files"][1]["path"], "src/second.txt");
    assert_eq!(result["files"][1]["status"], "completed");
}

// Catches: overlapping occurrences and overlapping chunks select an arbitrary span.
#[test]
fn multifile_edit_rejects_overlapping_original_spans() {
    for (before, chunks) in [
        ("aaa", json!([{"old_string":"aa","new_string":"x"}])),
        ("ééé", json!([{"old_string":"éé","new_string":"x"}])),
        (
            "abcd",
            json!([{"old_string":"abc","new_string":"x"},{"old_string":"bcd","new_string":"y"}]),
        ),
    ] {
        let ws = Workspace::in_mode(Mode::Edits);
        let path = ws.file("a", before);
        assert!(
            ws.call("edit", json!({"files":[{"path":"a","chunks":chunks}]}))
                .is_err()
        );
        assert_eq!(fs::read_to_string(path).unwrap(), before);
    }
}

// Catches: invalid batch shape or aliases survive preparation and affect files.
#[test]
fn multifile_edit_rejects_invalid_counts_and_canonical_duplicates() {
    let ws = Workspace::in_mode(Mode::Edits);
    let path = ws.file("a", "old");
    let chunk = json!({"old_string":"old","new_string":"new"});
    for files in [
        json!([]),
        json!([{"path":"a","chunks":[]}]),
        json!([{"path":"a","chunks":vec![chunk.clone();17]}]),
        json!([{"path":"a","chunks":[chunk,chunk],"replace_all":true}]),
        json!([{"path":"a","chunks":[chunk]},{"path":"./a","chunks":[chunk]}]),
    ] {
        assert!(ws.prepare("edit", json!({"files":files})).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
    }
}

// Catches: the file limit is absent or rejects the valid boundary.
#[test]
fn multifile_edit_file_count_boundary_uses_distinct_existing_files() {
    let ws = Workspace::in_mode(Mode::Edits);
    let files: Vec<_> = (0..17)
        .map(|index| {
            let path = format!("file-{index}.txt");
            ws.file(&path, "old");
            json!({"path":path,"chunks":[{"old_string":"old","new_string":"new"}]})
        })
        .collect();
    assert!(ws.prepare("edit", json!({"files": &files[..16]})).is_ok());
    assert!(ws.prepare("edit", json!({"files": &files})).is_err());
    for index in 0..17 {
        assert_eq!(
            fs::read_to_string(ws.root.join(format!("file-{index}.txt"))).unwrap(),
            "old"
        );
    }
}

// Catches: failure in a later file discards prior recovery evidence or edits later files.
#[test]
fn multifile_edit_stops_and_retains_completed_write_evidence() {
    let ws = Workspace::in_mode(Mode::Edits);
    let first = ws.file("a", "old");
    let second = ws.file("b", "different");
    let third = ws.file("c", "old");
    let error = ws
        .call_result(
            "edit",
            json!({"files":[
                {"path":"a","chunks":[{"old_string":"old","new_string":"new"}]},
                {"path":"b","chunks":[{"old_string":"old","new_string":"new"}]},
                {"path":"c","chunks":[{"old_string":"old","new_string":"new"}]}
            ]}),
        )
        .unwrap_err();
    let ToolError::WriteFailed(result) = error else {
        panic!("expected structured partial failure")
    };
    let value: serde_json::Value = serde_json::from_str(&result.model_text()).unwrap();
    assert_eq!(value["files"][0]["status"], "completed");
    assert_eq!(value["files"][1]["status"], "failed");
    assert_eq!(value["files"][2]["status"], "unattempted");
    assert_eq!(result.file_writes().len(), 1);
    assert_eq!(result.file_writes()[0].after_image.as_deref(), Some("new"));
    assert_eq!(fs::read_to_string(first).unwrap(), "new");
    assert_eq!(fs::read_to_string(second).unwrap(), "different");
    assert_eq!(fs::read_to_string(third).unwrap(), "old");
}

// Catches: a later denied file allows an earlier batch item to change.
#[test]
fn multifile_edit_denial_precedes_every_file_effect() {
    for deny in ["read(b)", "write(b)"] {
        let rules = Ruleset::from_layer(
            PermCfg {
                judge: None,
                allow: vec![Rule::load("write(**)").unwrap()],
                deny: vec![Rule::load(deny).unwrap()],
            },
            Source::Project,
        );
        let ws = Workspace::with_rules(Mode::Edits, rules);
        let first = ws.file("a", "old");
        let second = ws.file("b", "old");
        let call = ws.prepare("edit", json!({"files":[{"path":"a","chunks":[{"old_string":"old","new_string":"new"}]},{"path":"b","chunks":[{"old_string":"old","new_string":"new"}]}]})).unwrap();
        assert!(ws.authorize(call).is_err(), "{deny}");
        assert_eq!(fs::read_to_string(first).unwrap(), "old");
        assert_eq!(fs::read_to_string(second).unwrap(), "old");
    }
}

// Catches: the after-image byte limit drops the exact boundary or retains an oversized image.
#[test]
fn multifile_edit_after_image_exact_64k_boundary() {
    for size in [65_536, 65_537] {
        let ws = Workspace::in_mode(Mode::Edits);
        let path = ws.file("target", "old");
        let after = "x".repeat(size);
        let result = ws.call_result("edit", json!({"files":[{"path":"target","chunks":[{"old_string":"old","new_string":after}]}]})).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), after);
        assert_eq!(result.file_writes().len(), 1);
        assert_eq!(
            result.file_writes()[0].after_image.as_deref(),
            if size == 65_536 {
                Some(after.as_str())
            } else {
                None
            }
        );
    }
}

// Fault injection after the real checkpoint adapter replaces the file; not an OS fsync test.
#[test]
fn multifile_edit_post_replacement_failure_retains_recovery_evidence() {
    use ego_core::permit::{Fs, FsError, FsWriteAttempt, PathResolver, Scan};
    use ego_tools::checkpoint::CheckpointFs;
    struct FailAfterCommit(CheckpointFs);
    impl PathResolver for FailAfterCommit {
        fn canonicalize(&self, path: &std::path::Path) -> Result<CanonPath, FsError> {
            self.0.canonicalize(path)
        }
    }
    impl Fs for FailAfterCommit {
        fn read(&self, path: &CanonPath) -> Result<Vec<u8>, FsError> {
            self.0.read(path)
        }
        fn read_bounded(&self, path: &CanonPath, limit: usize) -> Result<Vec<u8>, FsError> {
            self.0.read_bounded(path, limit)
        }
        fn scan(&self, req: &ScanRequest) -> Result<Scan, FsError> {
            self.0.scan(req)
        }
        fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), FsError> {
            self.0.write(path, bytes)
        }
        fn write_attempt(&self, path: &CanonPath, bytes: &[u8]) -> FsWriteAttempt {
            let attempt = self.0.write_attempt(path, bytes);
            assert!(attempt.result.is_ok());
            FsWriteAttempt {
                result: Err(FsError::Io {
                    path: path.clone(),
                    message: "injected post-replacement failure".into(),
                }),
                checkpoint: attempt.checkpoint,
            }
        }
    }
    let ws = Workspace::in_mode(Mode::Yolo);
    let init = std::process::Command::new("git")
        .arg("-C")
        .arg(&ws.root)
        .args(["init", "--quiet"])
        .output()
        .unwrap();
    assert!(init.status.success());
    let path = ws.file("target", "before");
    let session = SessionId::new();
    let filesystem = FailAfterCommit(CheckpointFs::new(session));
    let tool = ws.tool("edit");
    let call = tool.prepare(json!({"files":[{"path":"target","chunks":[{"old_string":"before","new_string":"after"}]}]}), &ws.ctx()).unwrap();
    let (call, permit) = ws.authorize(call).unwrap().into_parts();
    let guarded = GuardedEffects::new(
        permit,
        &filesystem,
        &ws.process,
        &ws.evidence,
        Arc::new(NoResolver),
        Arc::new(NoHop),
        Arc::new(NoDuplex),
    );
    let failure = block_on(tool.execute(call, &guarded))
        .unwrap_err()
        .into_failed_result();
    assert_eq!(failure.file_writes().len(), 1);
    let entry = &failure.file_writes()[0];
    assert_eq!(
        entry.before_image,
        Some(ego_core::permit::BeforeImage::Text("before".into()))
    );
    assert!(entry.possibly_modified);
    assert_eq!(entry.after_image, None);
    assert_eq!(fs::read_to_string(&path).unwrap(), "after");
    let checkpoint = entry
        .checkpoint
        .as_ref()
        .expect("checkpoint from this attempt");
    CheckpointFs::restore(&ws.root, session, checkpoint).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "before");
}
