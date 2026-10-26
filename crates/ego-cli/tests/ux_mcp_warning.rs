#![cfg(unix)]
mod support;
use support::{Ego, Plan, Stub};

// Catches: a configured server that cannot start disappears from the live
// session without a visible cause, or leaks its raw configuration.
#[test]
fn interactive_start_reports_a_configured_mcp_server_that_cannot_start() {
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
        !result.stdout.contains("configured but not attached"),
        "{}",
        result.stdout
    );
    assert_eq!(
        result.stdout.matches("MCP server 'missing'").count(),
        1,
        "{}",
        result.stdout
    );
    assert!(
        support::notice_prose(&result.stdout).contains("ego mcp ls"),
        "{}",
        result.stdout
    );
    assert!(
        !result.stdout.contains(&missing.display().to_string()),
        "unsafe raw configuration diagnostic"
    );
}
