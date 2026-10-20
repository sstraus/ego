use super::*;

// Catches: every plan update is committed instead of repainting the live plan.
#[test]
fn pinned_plan_updates_commit_only_the_final_block() {
    pinned_plan_contract(80);
}

// Catches: wrapped plans exhaust the live height and strand the composer.
#[test]
fn pinned_plan_narrow_terminal_keeps_the_composer_live() {
    pinned_plan_contract(32);
}

fn pinned_plan_contract(cols: u16) {
    let updates = [
        serde_json::json!({"operation":"replace", "steps":[
            {"id":"a","text":"Inspect the current implementation and its callers", "status":"in_progress"},
            {"id":"b","text":"Implement the bounded live plan rendering", "status":"pending"},
            {"id":"c","text":"Verify the final terminal history", "status":"pending"}
        ]}),
        serde_json::json!({"operation":"step","id":"a","status":"completed"}),
        serde_json::json!({"operation":"step","id":"b","status":"completed"}),
        serde_json::json!({"operation":"step","id":"c","status":"in_progress"}),
    ];
    let (held, gate) = gated_paced_line("PINNED-PLAN-HELD\n");
    let mut completions: Vec<_> = updates
        .iter()
        .enumerate()
        .map(|(i, update)| Reply::Sse(tool_call(&format!("plan-{i}"), "plan", &update.to_string())))
        .collect();
    completions.push(held);
    let stub = Stub::start(Plan {
        completions,
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# pinned plan fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 12, cols, Some((12, cols)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"pin the plan\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"PINNED-PLAN-HELD");
    drain_until_quiet(&reader, &mut output);
    let mut parser = vt100::Parser::new(12, cols, 10_000);
    parser.process(&output);
    let busy = parser.screen().contents();
    let processed = output.len();
    // Let another timed repaint run while the provider remains held.
    until_normalized(&reader, &mut output, processed, "Working ");
    let end = output.len();
    gate.release();
    until_idle_status_ready(&reader, &mut output, end);
    drain_until_quiet(&reader, &mut output);
    parser.process(&output[processed..]);
    let live = parser.screen().contents();
    parser.screen_mut().set_scrollback(usize::MAX);
    let depth = parser.screen().scrollback();
    let mut history = String::new();
    for offset in (1..=depth).rev() {
        parser.screen_mut().set_scrollback(offset);
        history.push_str(&parser.screen().rows(0, cols).next().unwrap());
        history.push('\n');
    }
    history.push_str(&live);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(history.matches("Plan ").count(), 1, "{history}");
    let plan = busy.find("Plan 2/3").expect(&busy);
    let working = busy.find("Working").expect(&busy);
    assert!(plan < working, "{busy}");
    assert_eq!(busy.matches("Plan ").count(), 1, "{busy}");
    assert!(busy.lines().any(|row| row.trim() == ">"), "{busy}");
    assert!(!live.contains("Working"), "{live}");
}
