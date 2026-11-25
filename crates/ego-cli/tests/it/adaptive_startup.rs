//! Derived Adaptive roots and configured extra roots are independent authorities.
#![cfg(target_os = "macos")]
use crate::support;
use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text};

// Catches: configured root ceilings reject required derived cache/temp roots at startup.
#[test]
fn adaptive_startup_does_not_treat_derived_caches_as_configured_extra_roots() {
    for acp in [false, true] {
        let places = Places::new();
        std::fs::write(
            places.home_path().join("config.toml"),
            "model='ollama/qwen3'\nthinking='off'\n[profile.machine]\nroots=[]\n",
        )
        .unwrap();
        places.file(".ego/config.toml", "roots=[]\n");
        let stub = Stub::start(Plan {
            completions: vec![Reply::Sse(text("startup complete"))],
            ..Plan::default()
        });
        if acp {
            let mut host = AcpHost::adopt(
                Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
                    .subcommand("acp")
                    .spawn_conversing(),
            );
            host.initialize();
            let opened = host.request(
                "session/new",
                serde_json::json!({"cwd": places.workspace_path(), "mcpServers": [],
                "_meta": {"ego": {"ceilingProfile": "machine", "trust": "trusted"}}}),
            );
            let session = opened["result"]["sessionId"]
                .as_str()
                .expect("session admitted");
            let result = host.request(
                "session/prompt",
                serde_json::json!({"sessionId": session,
                "prompt": [{"type": "text", "text": "Hello"}]}),
            );
            assert!(result["result"].is_object(), "{result}");
            host.finish();
        } else {
            let result = places
                .ego(&stub)
                .conversing()
                .args(["--trust-workspace", "Hello"])
                .run_pty_seeded();
            assert_eq!(
                result.code,
                Some(0),
                "stdout: {} stderr: {}",
                result.stdout,
                result.stderr
            );
            assert!(
                result.stdout.contains("startup complete"),
                "{}",
                result.stdout
            );
        }
        assert_eq!(stub.generations().len(), 1);
    }
}
