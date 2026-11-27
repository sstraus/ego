use super::*;

const PRIVATE_PROMPT: &str = "EXPORTCTL-PRIVATE-PROMPT-7731";
const HELD_HEAD: &str = "EXPORT-CONTROL-HELD";
const FINAL_TAIL: &str = "EXPORTCTL-REPLY-FINISHED";
const RECEIPT: &str = "to this transcript; no file written.";

// Terminal wraps and gutters are not part of the exported bytes.
// The working-row spinner repaints between output chunks; it is not export bytes.
fn squashed(text: &str) -> String {
    text.lines()
        .filter(|line| !line.contains("Esc to interrupt"))
        .flat_map(str::chars)
        .filter(|c| !c.is_whitespace())
        .collect()
}

fn tree_names(root: &std::path::Path) -> Vec<String> {
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read workspace directory") {
            let entry = entry.expect("workspace directory entry");
            let path = entry.path();
            out.push(path.display().to_string());
            if entry.file_type().expect("workspace entry type").is_dir() {
                walk(&path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

// Catches: `/export` typed during a held reply is refused or queued until the
// turn ends, renders another snapshot than the committed prefix (missing the
// accepted prompt or leaking the unfinished reply), leaks the private prompt
// in a sanitized bundle, writes a file or journal bytes, or reaches the model.
#[test]
fn native_export_renders_committed_prefix_in_every_format_before_held_reply_release() {
    let gate = ReleaseGate::new();
    let Reply::Paced { head, .. } = paced_line(&format!("{HELD_HEAD}\n"), Duration::ZERO) else {
        unreachable!()
    };
    let stub = Stub::start(Plan {
        completions: vec![Reply::GatedPaced {
            head,
            tail: text(FINAL_TAIL),
            gate: Arc::clone(&gate),
        }],
        ..Plan::default()
    });
    struct ReleaseOnDrop(Arc<ReleaseGate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let _release = ReleaseOnDrop(Arc::clone(&gate));
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
        "--session-id",
        support::FIXTURE_SESSION_ID,
    ]);
    let journal = ego
        .home_path()
        .join("sessions")
        .join(support::FIXTURE_SESSION_ID)
        .join("events.jsonl");
    let workspace = ego.workspace_path().to_path_buf();
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, Some((40, 120)));
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input
        .write_all(format!("hold the reply for {PRIVATE_PROMPT}\r").as_bytes())
        .unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, HELD_HEAD.as_bytes());
    let deadline = std::time::Instant::now() + PROGRESS_DEADLINE;
    while stub.generations().is_empty() {
        output.extend(
            reader
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .expect("held generation started"),
        );
    }
    let journal_before = std::fs::read(&journal).expect("read session journal before exports");
    let tree_before = tree_names(&workspace);
    for (command, sanitized, markdown, kind, format) in [
        ("/export\r", true, true, "sanitized evidence bundle", "md"),
        (
            "/export sanitized json\r",
            true,
            false,
            "sanitized evidence bundle",
            "json",
        ),
        (
            "/export sanitized jsonl\r",
            true,
            false,
            "sanitized evidence bundle",
            "jsonl",
        ),
        ("/export local md\r", false, true, "local session", "md"),
        (
            "/export local json\r",
            false,
            false,
            "local session",
            "json",
        ),
        (
            "/export local jsonl\r",
            false,
            false,
            "local session",
            "jsonl",
        ),
    ] {
        let start = output.len();
        input.write_all(command.as_bytes()).unwrap();
        input.flush().unwrap();
        until_normalized_without_new_generation(&reader, &mut output, start, RECEIPT, &stub, 1);
        let window = normalized(&output[start..]);
        let flat = squashed(&window);
        assert!(
            window.contains(&format!("Exported {kind} as {format} {RECEIPT}")),
            "{command:?} receipt: {window:?}"
        );
        assert!(
            !window.contains("cannot export during a reply"),
            "{command:?} busy refusal: {window:?}"
        );
        assert!(
            !flat.contains(FINAL_TAIL),
            "{command:?} showed the unfinished reply: {window:?}"
        );
        let headline = if sanitized {
            "#egosanitizedexport"
        } else {
            "#egoexport"
        };
        assert_eq!(flat.contains(headline), markdown, "{command:?}: {window:?}");
        if sanitized {
            assert!(
                !flat.contains(PRIVATE_PROMPT),
                "{command:?} leaked the private prompt: {window:?}"
            );
            assert!(flat.contains("redactions"), "{command:?}: {window:?}");
        } else {
            assert!(
                flat.contains(PRIVATE_PROMPT),
                "{command:?} lacks the committed prompt: {window:?}"
            );
        }
    }
    assert_eq!(
        std::fs::read(&journal).expect("read session journal after exports"),
        journal_before,
        "export wrote journal bytes"
    );
    assert_eq!(tree_names(&workspace), tree_before, "export wrote a file");
    assert_eq!(stub.generations().len(), 1, "export reached the model");
    let start = output.len();
    gate.release();
    until_prompt_from(&reader, &mut output, start);
    assert!(normalized(&output).contains(FINAL_TAIL));
    let start = output.len();
    input.write_all(b"/export local jsonl\r").unwrap();
    input.flush().unwrap();
    until_normalized_without_new_generation(&reader, &mut output, start, RECEIPT, &stub, 1);
    assert!(
        squashed(&normalized(&output[start..])).contains(FINAL_TAIL),
        "idle export lacks the completed reply"
    );
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}
