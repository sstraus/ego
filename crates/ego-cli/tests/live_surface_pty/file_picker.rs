use super::*;

// Catches: accepting a path submits the prompt or replaces text after the cursor.
#[test]
fn file_picker_selects_at_cursor_without_submitting() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("FILE-PICKER-REPLY"))],
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("picked 0.txt", "other candidate");
    ego.file("picked file.txt", "content must not be read");
    let mut session = modal_session_on(ego, stub);
    session.press(b"before  after\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D@picked");
    session.wait("picked file.txt");
    session.press(b"\x1b[B");
    session.press(b"\r");
    drain_until_quiet(&session.reader, &mut session.output);
    assert!(
        session.stub.generations().is_empty(),
        "selection submitted the draft: {}",
        normalized(&session.output)
    );
    session.press(b"\r");
    session.wait("FILE-PICKER-REPLY");
    let (generations, _) = session.finish();
    assert_eq!(
        count_exact_strings(
            &generations[0]["messages"],
            "before @'picked file.txt' after"
        ),
        1,
        "actual messages: {}",
        generations[0]["messages"]
    );
}

// Catches: typing a space closes the menu before the full file name can match.
#[test]
fn file_picker_query_accepts_spaces() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("SPACED-PICKER-REPLY"))],
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("picked file.txt", "body");
    let mut session = modal_session_on(ego, stub);
    session.press(b"@picked file");
    session.wait("PICKED FILE.TXT");
    session.press(b"\r");
    assert!(session.stub.generations().is_empty());
    session.press(b"\r");
    session.wait("SPACED-PICKER-REPLY");
    let (generations, _) = session.finish();
    assert_eq!(
        count_exact_strings(&generations[0]["messages"], "@'picked file.txt'"),
        1
    );
}

// Catches: a path deleted after rendering is inserted from stale completion data.
#[test]
fn file_picker_rejects_a_removed_file() {
    let stub = Stub::start(Plan::default());
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    let path = ego.file("removed-picker.txt", "body");
    let mut session = modal_session_on(ego, stub);
    session.press(b"@removed");
    session.wait("REMOVED-PICKER.TXT");
    std::fs::remove_file(path).unwrap();
    session.press(b"\r");
    let screen = session.screen().join("\n");
    assert!(screen.contains("@removed"), "{screen}");
    assert!(!screen.contains("@removed-picker.txt"), "{screen}");
    assert!(session.stub.generations().is_empty());
    session.press(b"\x15");
    session.finish();
}

// Catches: ignored paths or symlink targets are disclosed by the terminal menu.
#[cfg(unix)]
#[test]
fn file_picker_does_not_list_ignored_or_external_files() {
    let stub = Stub::start(Plan::default());
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file(".gitignore", "secret-hidden.txt\n");
    ego.file("secret-hidden.txt", "hidden body");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("outside"), "private body").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("outside"),
        ego.workspace_path().join("secret-link.txt"),
    )
    .unwrap();
    let mut session = modal_session_on(ego, stub);
    session.press(b"@secret");
    session.wait("NO RECORDS FOUND");
    let screen = session.screen().join("\n");
    assert!(!screen.contains("secret-hidden.txt"), "{screen}");
    assert!(!screen.contains("secret-link.txt"), "{screen}");
    session.press(b"\x1b");
    session.press(b"\x15");
    session.finish();
}

// Catches: prose typed after an @ is still treated as one file query, so the menu stays open
// with NO RECORDS FOUND under a sentence that is no longer a mention.
#[test]
fn prose_after_an_at_sign_closes_the_file_menu() {
    let stub = Stub::start(Plan::default());
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("present.txt", "candidate");
    let mut session = modal_session_on(ego, stub);
    session.press(b"@ora siamo piu vicini");
    session.wait("vicini");
    let screen = session.screen().join("\n");
    assert!(!screen.contains("NO RECORDS FOUND"), "{screen}");
    session.press(b"\x15");
    session.finish();
}

// Catches: Escape submits the draft or replaces the owner's existing text.
#[test]
fn file_picker_escape_keeps_the_draft() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("FILE-CANCEL-REPLY"))],
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("cancel-file.txt", "private body");
    let mut session = modal_session_on(ego, stub);
    session.press(b"draft @cancel");
    session.wait("CANCEL-FILE.TXT");
    session.press(b"\x1b");
    drain_until_quiet(&session.reader, &mut session.output);
    assert!(session.stub.generations().is_empty());
    session.press(b"\r");
    session.wait("FILE-CANCEL-REPLY");
    let (generations, _) = session.finish();
    assert_eq!(
        count_exact_strings(&generations[0]["messages"], "draft @cancel"),
        1
    );
}
