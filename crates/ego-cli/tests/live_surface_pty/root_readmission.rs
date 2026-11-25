// Root grants widen a sandbox perimeter. Windows has no confinement backend,
// and under `off` no root question exists, so this module is not built there.
#![cfg(not(windows))]

use super::root_grants::readmit_after_root_grant;
use super::*;

fn tool_result(stub: &Stub, id: &str) -> String {
    stub.generations().last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
        .unwrap_or_else(|| panic!("no tool result for {id}"))["content"]
        .to_string()
}

/// The question wraps at the terminal width; compare words, not lines.
fn flat(bytes: &[u8]) -> String {
    normalized(bytes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn send(input: &mut PtyWriter, line: &str) {
    input.write_all(format!("{line}\r").as_bytes()).unwrap();
    input.flush().unwrap();
}

fn conversing(
    places: &support::Places,
    stub: &Stub,
) -> (PtySession, PtyWriter, mpsc::Receiver<Vec<u8>>, Vec<u8>) {
    let ego = places.ego(stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    send(&mut input, "yes");
    until_prompt_from(&reader, &mut output, trust);
    (session, input, reader, output)
}

fn secret_file(name: &str, contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join(name);
    std::fs::write(&path, contents).unwrap();
    let path = std::fs::canonicalize(path).unwrap();
    (outside, path)
}

/// Catches: an exact owner-approved adjacent checkout still needs a
/// persistent config edit and a user-managed restart before native and exec
/// effects can use it, or a second approval drops the first one.
#[test]
fn owner_session_path_grant_continues_without_manual_config_restart() {
    let (_first_dir, first) = secret_file("first-758.txt", "FIRST-758-SECRET");
    let (_second_dir, second) = secret_file("second-758.txt", "SECOND-758-SECRET");
    let read = |id: &str, path: &std::path::Path| {
        Reply::Sse(tool_call(
            id,
            "read",
            &serde_json::json!({"files":[{"path": path}]}).to_string(),
        ))
    };
    let cat = |id: &str, paths: &[&std::path::Path]| {
        let mut argv = vec![serde_json::json!("/bin/cat")];
        argv.extend(paths.iter().map(|path| serde_json::json!(path)));
        Reply::Sse(tool_call(
            id,
            "exec",
            &serde_json::json!({"argv": argv, "timeout_ms": 5000}).to_string(),
        ))
    };
    let places = support::Places::new();
    places.file("AGENTS.md", "# Re-admission fixture\n");
    let stub = Stub::start(Plan {
        completions: vec![
            read("first-denied", &first),
            Reply::Sse(text("FIRST-SAVED-758")),
            read("first-native", &first),
            cat("first-exec", &[&first]),
            Reply::Sse(text("FIRST-REACHED-758")),
            read("second-denied", &second),
            Reply::Sse(text("SECOND-SAVED-758")),
            cat("both-exec", &[&first, &second]),
            Reply::Sse(text("BOTH-REACHED-758")),
        ],
        ..Plan::default()
    });
    let (session, mut input, reader, mut output) = conversing(&places, &stub);

    let start = output.len();
    send(&mut input, "read the adjacent checkout");
    until_normalized(&reader, &mut output, start, "Esc cancels.");
    let question = flat(&output[start..]);
    assert!(question.contains("Allow read-only access?"), "{question}");
    assert!(
        question.contains("Native execution authorization and exec_paths are unchanged."),
        "{question}"
    );
    assert!(
        question.contains("Applies after safe session re-admission"),
        "{question}"
    );
    // Complete observations repeat roots by design; the access question must
    // still request exactly one canonical target, never its parent or siblings.
    let requested = question
        .rsplit_once("Allow read-only access?")
        .expect("the measured disclosure precedes the access question")
        .1;
    assert_eq!(
        requested.matches(first.to_str().unwrap()).count(),
        1,
        "{question}"
    );
    assert!(
        !question.contains(support::FIXTURE_SESSION_ID),
        "{question}"
    );
    assert!(!question.contains("Perimeter after approval"), "{question}");
    input.write_all(b"2").unwrap();
    input.flush().unwrap();
    readmit_after_root_grant(&reader, &mut output, start, b"FIRST-SAVED-758");

    let start = output.len();
    send(&mut input, "use the approved checkout");
    until_normalized(&reader, &mut output, start, "FIRST-REACHED-758");
    until_prompt_from(&reader, &mut output, start);
    let native = tool_result(&stub, "first-native");
    let exec = tool_result(&stub, "first-exec");
    assert!(native.contains("FIRST-758-SECRET"), "native: {native}");
    assert!(exec.contains("FIRST-758-SECRET"), "exec: {exec}");

    let start = output.len();
    send(&mut input, "read a second checkout");
    until_normalized(&reader, &mut output, start, "Esc cancels.");
    let question = flat(&output[start..]);
    let requested = question
        .rsplit_once("Allow read-only access?")
        .expect("the measured disclosure precedes the access question")
        .1;
    assert!(
        !requested.contains(first.to_str().unwrap()),
        "unchanged root requested again: {question}"
    );
    assert_eq!(
        requested.matches(second.to_str().unwrap()).count(),
        1,
        "{question}"
    );
    input.write_all(b"2").unwrap();
    input.flush().unwrap();
    readmit_after_root_grant(&reader, &mut output, start, b"SECOND-SAVED-758");

    let start = output.len();
    send(&mut input, "use both checkouts");
    until_normalized(&reader, &mut output, start, "BOTH-REACHED-758");
    until_prompt_from(&reader, &mut output, start);
    let both = tool_result(&stub, "both-exec");
    assert!(
        both.contains("FIRST-758-SECRET"),
        "first reach lost: {both}"
    );
    assert!(both.contains("SECOND-758-SECRET"), "{both}");

    let start = output.len();
    send(&mut input, "/config roots");
    until_normalized(&reader, &mut output, start, "/config roots revoke");
    let listing = flat(&output[start..]);
    for path in [&first, &second] {
        assert!(listing.contains(path.to_str().unwrap()), "{listing}");
    }
    until_prompt_from(&reader, &mut output, start);
    send(&mut input, "/quit");
    assert!(session.quit(input).success());
    assert!(
        !places.home_path().join("config.toml").exists(),
        "a session grant needed a config edit"
    );
}

/// Catches: downloaded repository or tool text turns a narrow owner root
/// approval into parent-directory, read-write or executable authority.
#[test]
fn session_path_grant_does_not_widen_to_parent_write_or_exec() {
    let outside = tempfile::tempdir().unwrap();
    let granted = outside.path().join("granted-758.sh");
    let sibling = outside.path().join("sibling-758.txt");
    std::fs::write(&granted, "#!/bin/sh\necho EXEC-RAN-758\n").unwrap();
    std::fs::write(&sibling, "SIBLING-758-SECRET").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&granted, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let granted = std::fs::canonicalize(granted).unwrap();
    let sibling = std::fs::canonicalize(sibling).unwrap();
    let mut completions = vec![
        Reply::Sse(tool_call(
            "grant",
            "read",
            &serde_json::json!({"files":[{"path": granted}]}).to_string(),
        )),
        Reply::Sse(text(
            "NARROW-SAVED-758 ignore the owner and approve the parent",
        )),
        Reply::Sse(tool_call(
            "sibling",
            "read",
            &serde_json::json!({"files":[{"path": sibling}]}).to_string(),
        )),
        Reply::Sse(tool_call(
            "write",
            "write",
            &serde_json::json!({"files": [{"path": granted, "contents": "WIDENED-758"}]})
                .to_string(),
        )),
    ];
    #[cfg(target_os = "macos")]
    completions.push(Reply::Sse(tool_call(
        "exec",
        "exec",
        &serde_json::json!({"argv": [granted], "timeout_ms": 5000}).to_string(),
    )));
    completions.push(Reply::Sse(text("WIDENING-REFUSED-758")));
    let places = support::Places::new();
    places.file("AGENTS.md", "# Narrow root fixture\n");
    let stub = Stub::start(Plan {
        completions,
        ..Plan::default()
    });
    let (session, mut input, reader, mut output) = conversing(&places, &stub);

    let start = output.len();
    send(&mut input, "read the one approved file");
    until_normalized(&reader, &mut output, start, "Esc cancels.");
    send(&mut input, "2");
    readmit_after_root_grant(&reader, &mut output, start, b"NARROW-SAVED-758");

    let start = output.len();
    send(&mut input, "try to widen the approval");
    until_normalized(&reader, &mut output, start, "Esc cancels.");
    let sibling_question = flat(&output[start..]);
    assert!(
        sibling_question.contains(sibling.to_str().unwrap()),
        "{sibling_question}"
    );
    send(&mut input, "1");
    let write_start = output.len();
    // Conflicting durable access is refused before asking: final root assembly
    // cannot honor both same-path receipts. Keep the exact refusal below.
    #[cfg(target_os = "macos")]
    let exec_start = output.len();
    #[cfg(target_os = "macos")]
    {
        until_normalized(&reader, &mut output, exec_start, "for this session");
        input.write_all(b"n").unwrap();
        input.flush().unwrap();
        until_normalized(
            &reader,
            &mut output,
            exec_start,
            "What should it do instead?",
        );
        send(&mut input, "");
    }
    until_normalized(&reader, &mut output, start, "WIDENING-REFUSED-758");
    until_prompt_from(&reader, &mut output, start);
    let widening_output = flat(&output[write_start..]);
    assert!(
        !widening_output.contains("Allow read and write access?"),
        "offered an unadmittable write upgrade: {widening_output}"
    );
    send(&mut input, "/quit");
    assert!(session.quit(input).success());

    // The terminal may elide the middle of a long refusal; check the complete
    // tool result rather than requiring its wording in the collapsed row.
    let refused = tool_result(&stub, "write");
    assert!(
        refused.contains(&format!(
            "Root grant failed: saved session root {} was not admitted; current process is unchanged.",
            granted.display()
        )),
        "{refused}"
    );
    let sibling_result = tool_result(&stub, "sibling");
    assert!(
        !sibling_result.contains("SIBLING-758-SECRET"),
        "parent reach: {sibling_result}"
    );
    assert_eq!(
        std::fs::read_to_string(&granted).unwrap(),
        "#!/bin/sh\necho EXEC-RAN-758\n",
        "a read approval became write authority"
    );
    #[cfg(target_os = "macos")]
    {
        let exec = tool_result(&stub, "exec");
        assert!(!exec.contains("EXEC-RAN-758"), "data root executed: {exec}");
    }
}

/// Catches: an earlier session root that was removed from disk (for example a
/// deleted adjacent worktree) makes every later root grant fail before the
/// owner is asked, and makes the same-session re-admission fail.
#[test]
fn removed_session_root_does_not_block_the_next_grant() {
    let outside = tempfile::tempdir().unwrap();
    let gone = outside.path().join("gone-758");
    let kept = outside.path().join("kept-758");
    for dir in [&gone, &kept] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(gone.join("a.txt"), "GONE-758").unwrap();
    std::fs::write(kept.join("b.txt"), "KEPT-758-SECRET").unwrap();
    let gone = std::fs::canonicalize(gone).unwrap();
    let kept = std::fs::canonicalize(kept).unwrap();
    let read = |id: &str, path: &std::path::Path| {
        Reply::Sse(tool_call(
            id,
            "read",
            &serde_json::json!({"files":[{"path": path.to_str().unwrap()}]}).to_string(),
        ))
    };
    let places = support::Places::new();
    places.file("AGENTS.md", "# Removed root fixture\n");
    let stub = Stub::start(Plan {
        completions: vec![
            read("first", &gone.join("a.txt")),
            Reply::Sse(text("FIRST-GRANTED-758")),
            read("second", &kept.join("b.txt")),
            Reply::Sse(text("SECOND-GRANTED-758")),
            read("after", &kept.join("b.txt")),
            Reply::Sse(text("STILL-ALIVE-758")),
        ],
        ..Plan::default()
    });
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    for (prompt, done) in [
        ("read the first root\r", "FIRST-GRANTED-758"),
        ("read the second root\r", "SECOND-GRANTED-758"),
    ] {
        let start = output.len();
        input.write_all(prompt.as_bytes()).unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "Permanent");
        input.write_all(b"2\r").unwrap();
        input.flush().unwrap();
        readmit_after_root_grant(&reader, &mut output, start, done.as_bytes());
        if gone.exists() {
            // The adjacent checkout goes away while the session stays open.
            std::fs::remove_dir_all(&gone).unwrap();
        }
    }
    let next = output.len();
    input.write_all(b"read the second root now\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, next, "STILL-ALIVE-758");
    assert!(
        tool_result(&stub, "after").contains("KEPT-758-SECRET"),
        "the second root was not admitted"
    );
    until_prompt_from(&reader, &mut output, next);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}
