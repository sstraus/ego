//! A host ceiling belongs to the session from creation, before any prompt.

use crate::support;

use support::{AcpHost, Ego, Places};

fn host(places: &Places) -> AcpHost {
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .args(["--sandbox", "off"])
            .spawn_conversing(),
    );
    host.initialize();
    host
}

fn reopen_before_first_prompt(method: &str) {
    let places = Places::new();
    std::fs::write(
            places.home_path().join("config.toml"),
            "[profile.machine]\nmode='default'\nsandbox='off'\nroots=[]\n[profile.repo]\nmode='yolo'\nsandbox='off'\nroots=[]\n",
        )
        .unwrap();
    let mut first = host(&places);
    let created = first.request(
        "session/new",
        serde_json::json!({
            "cwd": places.workspace_path(), "mcpServers": [],
            "_meta": {"ego": {"profile": "repo", "ceilingProfile": "machine"}}
        }),
    );
    let session = created["result"]["sessionId"].as_str().unwrap().to_owned();
    assert_eq!(
        created["result"]["_meta"]["ego"]["effective"]["mode"],
        "default"
    );
    let closed = first.request("session/close", serde_json::json!({"sessionId": session}));
    assert!(closed.get("error").is_none(), "{closed}");
    first.finish();

    let mut reopened = host(&places);
    let loaded = reopened.request(
        method,
        serde_json::json!({"sessionId": session, "cwd": places.workspace_path(), "mcpServers": []}),
    );
    assert!(loaded.get("error").is_none(), "{method}: {loaded}");
    let widened = reopened.request(
        "session/set_config_option",
        serde_json::json!({"sessionId": session, "configId": "mode", "value": "yolo"}),
    );
    assert!(
        widened.get("error").is_some(),
        "{method} dropped the ceiling before the first prompt: {widened}"
    );
    let allowed = reopened.request(
        "session/set_config_option",
        serde_json::json!({"sessionId": session, "configId": "mode", "value": "default"}),
    );
    assert!(
        allowed["result"].is_object(),
        "controls unavailable: {allowed}"
    );
    reopened.finish();
}

// Catches: creation does not persist the ceiling, so load after restart exposes yolo.
#[test]
fn load_before_first_prompt_does_not_drop_session_ceiling() {
    reopen_before_first_prompt("session/load");
}

// Catches: resume after restart loses ceiling authority when no run exists yet.
#[test]
fn resume_before_first_prompt_does_not_drop_session_ceiling() {
    reopen_before_first_prompt("session/resume");
}
