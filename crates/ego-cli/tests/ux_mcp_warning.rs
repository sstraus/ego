#![cfg(unix)]
mod support;
use support::{Ego, Plan, Stub};

// Catches: an absent configured server silently disappears from the live session.
#[test]
fn interactive_start_warns_when_a_configured_mcp_server_is_absent() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let missing = ego.workspace_path().join("missing-server");
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.missing]\ntransport='stdio'\ncommand={:?}\n",
            missing
        ),
    );
    let result = ego
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off"])
        .run_pty("hello");
    assert_eq!(result.code, Some(0), "{}", result.stdout);
    assert!(
        result
            .stdout
            .contains("MCP server `missing`: configured but not attached."),
        "{}",
        result.stdout
    );
    assert!(result.stdout.contains("ego mcp ls"), "{}", result.stdout);
    assert!(
        !result.stdout.contains(&missing.display().to_string()),
        "unsafe raw configuration diagnostic"
    );
}
