#![cfg(unix)]

use super::*;
use std::os::unix::fs::PermissionsExt;

// Catches: each CLI prompt rebuilds TurnLoop and retries empty/failed warmup.
#[test]
fn mdkb_warmup_attempt_survives_interactive_turn_reconstruction() {
    for outcome in ["printf '{}\\n'", "exit 1", "/bin/sleep 5"] {
        let stub = Stub::start(Plan::default());
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
        ego.file("AGENTS.md", "# prompt context fixture\n");
        let log = ego.workspace_path().join("mdkb-inputs.jsonl");
        let script = ego.file("mdkb-fixture", &format!(
            "#!/bin/sh\ncat >> '{}'\nprintf '\\n' >> '{}'\nif [ \"$2\" = session-start ]; then {}; else printf '{{}}\\n'; fi\n",
            log.display(), log.display(), outcome,
        ));
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        ego.home_file(
            "config.toml",
            &format!("mdkb_prompt_command = {:?}\n", script),
        );
        let (session, mut input, reader) = PtySession::start(&ego, 24, 120, Some((24, 120)));
        let mut output = Vec::new();
        until(&reader, &mut output, b"Trust project files");
        input.write_all(b"yes\r").unwrap();
        input.flush().unwrap();
        until_post_trust_prompt(&reader, &mut output, 0);
        for prompt in [b"first prompt\r".as_slice(), b"second prompt\r".as_slice()] {
            let start = output.len();
            input.write_all(prompt).unwrap();
            input.flush().unwrap();
            until_normalized(&reader, &mut output, start, "It is done.");
            until_prompt_from(&reader, &mut output, start);
        }
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());
        let events: Vec<serde_json::Value> = std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(stub.generations().len(), 2, "{outcome}");
        assert_eq!(
            events
                .iter()
                .filter(|event| event["hook_event_name"] == "UserPromptSubmit")
                .count(),
            2,
            "{outcome}: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["hook_event_name"] == "SessionStart")
                .count(),
            1,
            "{outcome}: {events:?}"
        );
    }
}
