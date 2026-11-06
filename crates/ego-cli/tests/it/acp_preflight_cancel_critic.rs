#![cfg(unix)]

//! A host cancellation that lands while a continuation's repeated preflight is
//! still waiting on a credential command settles the prompt as cancelled and
//! leaves the session usable. No provider is contacted: the credential command
//! never returns.

use crate::support;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use ego_cli::auth::{AuthEntry, AuthStore};
use support::{AcpHost, Ego, Places};

const FACTS: &str = r#"
[model."openai-codex/gpt-5.1".capabilities]
schema_version = 6
max_context_tokens = 400000
max_output_tokens = 128000
max_tools = 128
supports_parallel_tool_calls = true
reasoning_carryover = "same_provider_only"
reasoning_efforts = "any"
default_reasoning_effort = "off"
cancel = "cooperative"
cache = "long"
transport = "sse"
"#;

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(path.exists(), "the credential command never started");
}

fn prompt(session: &str) -> serde_json::Value {
    serde_json::json!({"sessionId": session, "prompt": [{"type": "text", "text": "Ask"}]})
}

// Catches: a cancelled continuation whose preflight was waiting on a
// credential settling as an executor error, so the prompt fails instead of
// answering `cancelled` and the session refuses the next prompt as uncertain.
#[test]
fn cancel_during_continuation_preflight_settles_cancelled_twice_without_wedging() {
    let places = Places::new();
    let home = places.home_path().to_path_buf();
    let hang = home.join("hang");
    let started = home.join("started");
    let script = home.join("credential.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ -f '{hang}' ]; then : > '{started}'; sleep 30; fi\n\
             printf '%s\\n' '{{\"access\":\"head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig\",\"refresh\":\"r\",\"expires\":0}}'\n",
            hang = hang.display(),
            started = started.display(),
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(home.join("models.toml"), FACTS).unwrap();
    fs::write(
        home.join("config.toml"),
        format!(
            "model = \"openai-codex/gpt-5.1\"\nsandbox = \"workspace\"\n[auth.openai-codex]\ncredential_command = \"{}\"\n",
            script.display()
        ),
    )
    .unwrap();
    AuthStore::new(&home)
        .put(
            "openai-codex",
            AuthEntry::OAuth {
                access: "old".into(),
                refresh: Some("old".into()),
                expires: Some(0),
            },
        )
        .unwrap();
    let mut host = AcpHost::adopt(
        Ego::at(&home, places.workspace_path(), None)
            .subcommand("acp")
            .spawn_conversing(),
    );
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().unwrap());
    fs::write(&hang, "").unwrap();

    for round in 0..2 {
        let _ = fs::remove_file(&started);
        let id = host.send_request("session/prompt", prompt(&session));
        wait_for(&started);
        host.write_frame(serde_json::json!({
            "jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session},
        }));
        let answer = loop {
            let frame = host.read_frame();
            if frame["id"] == serde_json::json!(id) {
                break frame;
            }
        };
        assert_eq!(
            answer["result"]["stopReason"], "cancelled",
            "round {round}: {answer}"
        );
    }
    host.finish();
}
