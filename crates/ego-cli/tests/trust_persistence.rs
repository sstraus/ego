//! contract for regression case: the trust question is asked when there is
//! something to trust, and only until it has been answered.
//!
//! Driven through a real pseudo-terminal because the question only exists
//! where a human seat does. See
//! `docs/decisions/2026-09-05-a-trust-answer-is-remembered-per-root.md`.

mod support;

use std::fs;

use support::{Ego, Places, Plan, Reply, Stub, text};

const QUESTION: &str = "Trust project files";

fn stub() -> Stub {
    Stub::start(Plan {
        completions: (0..4).map(|_| Reply::Sse(text("trust fixture"))).collect(),
        ..Plan::default()
    })
}

fn launch(ego: Ego) -> String {
    ego.conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off"])
        .run_pty("hello")
        .stdout
}

#[test]
fn a_workspace_with_nothing_to_trust_is_never_asked_about() {
    let places = Places::new();
    let stub = stub();

    let launched = launch(places.ego(&stub));

    assert!(
        !launched.contains(QUESTION),
        "an empty workspace has no admission to grant:\n{launched}"
    );
}

#[test]
fn the_first_launch_asks_and_the_next_one_remembers_the_answer() {
    let places = Places::new();
    let stub = stub();
    places.ego(&stub).file("AGENTS.md", "# be careful here\n");

    let first = launch(places.ego(&stub));
    assert!(
        first.contains(QUESTION),
        "a workspace with instructions is asked about:\n{first}"
    );

    let second = launch(places.ego(&stub));
    assert!(
        !second.contains(QUESTION),
        "the same unchanged workspace is not asked about twice:\n{second}"
    );
}

#[test]
fn changed_instruction_content_does_not_reask() {
    let places = Places::new();
    let stub = stub();
    places.ego(&stub).file("AGENTS.md", "# be careful here\n");

    let first = launch(places.ego(&stub));
    assert!(first.contains(QUESTION), "{first}");

    places
        .ego(&stub)
        .file("AGENTS.md", "# be careful here, and also run this\n");
    let after_edit = launch(places.ego(&stub));

    assert!(
        !after_edit.contains(QUESTION),
        "an instruction edit is not new executable authority:\n{after_edit}"
    );
}

#[test]
fn terminal_reask_describes_changed_hook_and_program() {
    let places = Places::new();
    let stub = stub();
    places.ego(&stub).file("scripts/hook.sh", "echo old\n");
    places.ego(&stub).file(".ego/config.toml", "[[hooks]]\nevent='PreToolUse'\n[hooks.handler]\ntype='command'\ncommand='./scripts/hook.sh'\nargs=['--old']\n");
    let first = launch(places.ego(&stub));
    assert!(first.contains(QUESTION), "{first}");
    places
        .ego(&stub)
        .file("scripts/hook.sh", "echo changed and longer\n");
    places.ego(&stub).file(".ego/config.toml", "[[hooks]]\nevent='PreToolUse'\n[hooks.handler]\ntype='command'\ncommand='./scripts/hook.sh'\nargs=['--new']\n");
    let second = launch(places.ego(&stub));
    assert!(second.contains(QUESTION), "{second}");
    assert!(
        second.contains("Executable changes since the last answer"),
        "{second}"
    );
    assert!(
        second.contains(".ego/config.toml") && second.contains("PreToolUse#1"),
        "{second}"
    );
    assert!(
        second.contains("--old") && second.contains("--new"),
        "{second}"
    );
    assert!(
        second.contains("size=9") && second.contains("size=24"),
        "{second}"
    );
}

#[test]
fn bare_neither_asks_nor_consults_the_remembered_answer() {
    let places = Places::new();
    let stub = stub();
    places.ego(&stub).file("AGENTS.md", "# be careful here\n");

    let launched = places
        .ego(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off", "--bare"])
        .run_pty("hello")
        .stdout;

    assert!(
        !launched.contains(QUESTION),
        "`--bare` is already an explicit refusal:\n{launched}"
    );
    assert!(
        !places.ego(&stub).home_path().join("trust.json").exists(),
        "a bare launch records no answer"
    );
}

#[test]
fn a_corrupt_trust_ledger_is_asked_about_again_without_being_overwritten() {
    let places = Places::new();
    let stub = stub();
    places.ego(&stub).file("AGENTS.md", "# be careful here\n");
    let path = places.ego(&stub).home_file("trust.json", "{ not json\n");
    let before = fs::read(&path).expect("the corrupt ledger is readable");

    let launched = places
        .ego(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off"])
        .run_pty_trusting("yes", "hello");

    assert_eq!(launched.code, Some(0), "{}", launched.stdout);
    assert!(
        launched.stdout.contains(QUESTION),
        "corrupt trust state must ask again:\n{}",
        launched.stdout
    );
    assert_eq!(
        fs::read(&path).expect("the corrupt ledger remains readable"),
        before,
        "a fresh answer must not silently replace corrupt trust state"
    );
}
