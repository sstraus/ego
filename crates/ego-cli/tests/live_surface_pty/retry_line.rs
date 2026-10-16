use super::*;

// Catches: raw provider errors and retry notes accumulate instead of replacing
// the live row, or recovery drops the cause instead of settling one receipt.
#[test]
fn three_provider_retries_replace_one_row_and_recovery_keeps_one_receipt() {
    retry_contract("xterm-256color", false, true);
}

// Catches: explicit plain output still replaces retry diagnostics.
#[test]
fn plain_retry_notices_are_appended() {
    retry_contract("xterm-256color", true, false);
}

// Catches: a dumb terminal receives live retry repaint state.
#[test]
fn dumb_retry_notices_are_appended() {
    retry_contract("dumb", false, false);
}

fn retry_contract(term: &str, plain: bool, live: bool) {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Status(503),
            Reply::Status(503),
            Reply::StatusRetryAfter {
                status: 503,
                seconds: 3,
            },
            Reply::Sse(text("RETRY-RECOVERED")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--retry",
            "3",
        ])
        .env("TERM", term);
    let ego = if plain { ego.arg("--plain") } else { ego };
    ego.file("AGENTS.md", "# retry surface fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 180, Some((40, 180)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"retry line\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "attempt 3/3");
    drain_until_quiet(&reader, &mut output);
    let before = screen_rows(&output, 40, 180).join("\n");
    let live_count = before.matches("503").count();
    let start = output.len();
    until_normalized(&reader, &mut output, start, "RETRY-RECOVERED");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let after = screen_rows(&output, 40, 180).join("\n");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    eprintln!("BEFORE SCREEN:\n{before}\nAFTER SCREEN:\n{after}");
    if !live {
        assert!(live_count >= 3, "plain notices must append: {before}");
        return;
    }
    assert_eq!(live_count, 1, "one live cause row: {before}");
    assert_eq!(
        after.matches("503").count(),
        1,
        "one settled cause: {after}"
    );
    assert!(!after.contains("retrying in"), "stale retry row: {after}");
}
