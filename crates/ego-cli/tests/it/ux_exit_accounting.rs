#![cfg(unix)]
use crate::support;
use support::{Ego, Plan, Stub};

// Catches: the exit note claims drafts are saved although DraftStash is in memory only.
#[test]
fn clean_exit_reports_drafts_as_discarded_not_saved() {
    let stub = Stub::start(Plan::default());
    let result = Ego::against(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off"])
        .run_pty("hello");
    assert_eq!(result.code, Some(0), "{}", result.stdout);
    assert!(
        result
            .stdout
            .contains("0 unsent drafts discarded; 0 prompts waiting to run."),
        "{}",
        result.stdout
    );
    assert!(!result.stdout.contains("accepted inputs"));
    assert!(
        !result.stdout.contains("Saved drafts"),
        "drafts live in memory only; the exit note must not claim persistence: {}",
        result.stdout
    );
}
