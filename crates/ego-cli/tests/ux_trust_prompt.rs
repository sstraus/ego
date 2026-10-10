#![cfg(unix)]
mod support;
use support::{Ego, Plan, Stub};

// Catches: consent offers relative files with no explanation of the two answers.
#[test]
fn trust_prompt_names_absolute_files_and_explains_yes_and_no() {
    for answer in ["yes", "no"] {
        let stub = Stub::start(Plan::default());
        let ego = Ego::against(&stub);
        let path = ego.file("AGENTS.md", "# project instructions\n");
        let path = path.canonicalize().unwrap().display().to_string();
        let result = ego
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"])
            .run_pty_trusting(answer, "hello");
        assert_eq!(result.code, Some(0), "{}", result.stdout);
        for expected in [
            "Project files offered for trust:",
            &path,
            "Yes: load project instructions, configuration, skills, hooks and MCP server declarations.",
            "No: continue without project files.",
        ] {
            assert!(
                result.stdout.contains(expected),
                "missing {expected}: {}",
                result.stdout
            );
        }
    }
}
