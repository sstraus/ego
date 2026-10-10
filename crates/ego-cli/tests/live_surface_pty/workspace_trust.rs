use super::*;

// Catches: a fresh managed worktree cannot explicitly admit project files and
// deliver its argv task without a trust answer consuming terminal input.
#[test]
fn managed_pty_explicit_trust_delivers_initial_prompt_once() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("REGRESSION-MARKER-ANSWER"))],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# REGRESSION-MARKER-PROJECT-INSTRUCTIONS\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--yolo",
        "--trust-workspace",
        "REGRESSION-MARKER-INITIAL-TASK",
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
    while !normalized(&output).contains("REGRESSION-MARKER-ANSWER") {
        match reader.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(chunk) => output.extend(chunk),
            Err(_) => break,
        }
    }
    let answered = normalized(&output).contains("REGRESSION-MARKER-ANSWER");
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
        count_exact_strings(&generations[0]["messages"], "REGRESSION-MARKER-INITIAL-TASK"),
        1
    );
    assert!(
        generations[0]["messages"]
            .to_string()
            .contains("REGRESSION-MARKER-PROJECT-INSTRUCTIONS")
    );
    assert!(
        ego.home_path().join("trust.json").exists(),
        "explicit answer must be remembered"
    );
}
