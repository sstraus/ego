#![cfg(unix)]
mod support;
use support::{Ego, Plan, Stub};

// Catches: the clean exit exposes unexplained 'accepted inputs' and 'unsent drafts'.
#[test]
fn clean_exit_names_saved_drafts_and_prompts_waiting_to_run() {
    let stub = Stub::start(Plan::default());
    let result = Ego::against(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off"])
        .run_pty("hello");
    assert_eq!(result.code, Some(0), "{}", result.stdout);
    assert!(
        result
            .stdout
            .contains("Saved drafts: 0; prompts waiting to run: 0."),
        "{}",
        result.stdout
    );
    assert!(!result.stdout.contains("accepted inputs"));
}
