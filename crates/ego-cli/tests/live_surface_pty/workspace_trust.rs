use super::*;

// Catches: the terminal seat ignores a valid linked-worktree answer and
// consumes a trust modal instead of delivering the argv task once.
#[cfg(unix)]
#[test]
fn linked_worktree_uses_main_trust_in_real_terminal_launch() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("TRUST-929-MAIN-ANSWER")),
            Reply::Sse(text("TRUST-929-CHILD-ANSWER")),
        ],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# TRUST-929-PROJECT-INSTRUCTIONS\n");
    let git = |args: &[&str]| {
        let result = std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(places.workspace_path())
            .args(args)
            .output()
            .expect("git fixture");
        assert!(
            result.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["add", "AGENTS.md"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-qm",
        "fixture",
    ]);
    let main = support::Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
        .args([
            "--model",
            "ollama/qwen3",
            "--thinking",
            "off",
            "--sandbox",
            "off",
            "--yolo",
            "--trust-workspace",
            "TRUST-929-MAIN-TASK",
        ])
        .run();
    assert_eq!(
        main.code,
        Some(0),
        "main launch: {} {}",
        main.stdout,
        main.stderr
    );
    let child = places.home_path().join("trust-929-child");
    git(&[
        "worktree",
        "add",
        "-qb",
        "trust-929-child",
        child.to_str().unwrap(),
    ]);
    let result = support::Ego::at(places.home_path(), &child, Some(&stub))
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--thinking",
            "off",
            "--sandbox",
            "off",
            "--yolo",
            "TRUST-929-CHILD-TASK",
        ])
        .run_pty_seeded();
    assert_eq!(result.code, Some(0), "linked terminal: {}", result.stdout);
    assert!(
        result.stdout.contains("TRUST-929-CHILD-ANSWER"),
        "argv task did not run: {}",
        result.stdout
    );
    assert!(
        !result.stdout.contains("Trust project files"),
        "linked checkout asked again: {}",
        result.stdout
    );
    assert_eq!(
        stub.generations().len(),
        2,
        "each launch must deliver exactly once"
    );
}

// Catches: a fresh managed worktree cannot explicitly admit project files and
// deliver its argv task without a trust answer consuming terminal input.
#[test]
fn managed_pty_explicit_trust_delivers_initial_prompt_once() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("TRUST-304-ANSWER"))],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# TRUST-304-PROJECT-INSTRUCTIONS\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--yolo",
        "--trust-workspace",
        "TRUST-304-INITIAL-TASK",
    ]);
    assert!(!ego.home_path().join("trust.json").exists());
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let mut child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    drop(pair.slave);
    let (mut input, dsr, writer_join) = writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(pair.master.try_clone_reader().expect("reader"), dsr);
    let mut output = Vec::new();
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while !normalized(&output).contains("TRUST-304-ANSWER") {
        match reader.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(chunk) => output.extend(chunk),
            Err(_) => break,
        }
    }
    let answered = normalized(&output).contains("TRUST-304-ANSWER");
    if answered {
        until_prompt_from(&reader, &mut output, 0);
        input.write_all(b"/quit\r").expect("quit");
        input.flush().expect("flush");
    } else {
        child.kill().expect("kill blocked launch");
    }
    let status = bounded_exit(child);
    drop(input);
    drop(pair.master);
    reader_join.join().expect("reader");
    writer_join.join().expect("writer");
    assert!(
        answered,
        "explicit trust did not deliver the task: {}",
        normalized(&output)
    );
    assert!(status.success(), "REPL exit: {status:?}");
    assert!(!normalized(&output).contains("Trust project files"));
    let generations = stub.generations();
    assert_eq!(generations.len(), 1, "initial task must run once");
    assert_eq!(
        count_exact_strings(&generations[0]["messages"], "TRUST-304-INITIAL-TASK"),
        1
    );
    assert!(
        generations[0]["messages"]
            .to_string()
            .contains("TRUST-304-PROJECT-INSTRUCTIONS")
    );
    assert!(
        ego.home_path().join("trust.json").exists(),
        "explicit answer must be remembered"
    );
}
