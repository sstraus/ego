//! contract for invocation MCP discovery and stdio execution
//! consume the resolved run confinement.

use crate::support;

use support::{Ego, FIXTURE_SESSION_ID, Plan, Stub, text};

fn http_config(address: &str) -> String {
    format!(
        "[mcp.servers.remote]\ntransport = 'http'\nurl = 'http://{address}/mcp'\nauth = 'none'\nallow_insecure_loopback = true\n"
    )
}

fn mcp_requests(stub: &Stub) -> Vec<support::Recorded> {
    stub.requests()
        .into_iter()
        .filter(|request| request.path == "/mcp")
        .collect()
}

fn run_kinds(finished: &support::Finished) -> Vec<String> {
    finished
        .durable_payloads()
        .into_iter()
        .filter_map(|payload| payload["event"]["kind"]["kind"].as_str().map(str::to_owned))
        .collect()
}

fn assert_no_durable_run_admission(finished: &support::Finished) {
    let kinds = run_kinds(finished);
    assert!(
        kinds
            .iter()
            .all(|kind| !kind.contains("Run") && !kind.contains("Execution")),
        "confinement refusal published durable run evidence: {kinds:?}"
    );
}

#[test]
fn offline_http_discovery_refuses_before_network_registry_or_run_admission() {
    let stub = Stub::start_mcp(Plan::default());
    let ego = Ego::against(&stub);
    ego.home_file("config.toml", &http_config(&stub.address));

    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            FIXTURE_SESSION_ID,
            "--output",
            "jsonl",
            "--offline",
            "--mcp",
            "remote",
            "Ask",
        ])
        .run();

    assert_ne!(finished.code, Some(0), "offline MCP run was admitted");
    assert!(
        mcp_requests(&stub).is_empty(),
        "offline refusal reached MCP discovery: {:?}",
        mcp_requests(&stub)
    );
    assert_no_durable_run_admission(&finished);
}

/// HTTP MCP runs in process, like the provider connection, so
/// a filesystem sandbox does not refuse an explicitly selected server.
#[test]
fn restrictive_sandbox_admits_selected_http_mcp() {
    let stub = Stub::start_mcp(Plan {
        completions: vec![support::Reply::Sse(text("done"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub);
    ego.home_file("config.toml", &http_config(&stub.address));

    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            FIXTURE_SESSION_ID,
            "--output",
            "jsonl",
            "--sandbox",
            "ro",
            "--mcp",
            "remote",
            "Ask",
        ])
        .run();

    assert_eq!(
        finished.code,
        Some(0),
        "restrictive sandbox refused the selected HTTP MCP server: {}",
        finished.stderr
    );
    assert!(
        !mcp_requests(&stub).is_empty(),
        "restrictive sandbox skipped HTTP MCP discovery"
    );
}

/// A plaintext server outside loopback connects when the owner
/// selects it. The IPv4-mapped literal is a host that the loopback rule refuses
/// (`Ipv6Addr::is_loopback` is false for it), yet it reaches the local fixture.
#[test]
fn explicit_non_loopback_http_mcp_selection_connects_under_the_default_sandbox() {
    let stub = Stub::start_mcp(Plan {
        completions: vec![support::Reply::Sse(text("done"))],
        ..Plan::default()
    });
    let port = stub.address.rsplit_once(':').expect("host:port").1;
    let ego = Ego::against(&stub);
    ego.home_file(
        "config.toml",
        &http_config(&format!("[::ffff:127.0.0.1]:{port}")),
    );

    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            FIXTURE_SESSION_ID,
            "--output",
            "jsonl",
            "--mcp",
            "remote",
            "Ask",
        ])
        .run();

    assert_eq!(
        finished.code,
        Some(0),
        "the selected non-loopback HTTP MCP server was refused: {}",
        finished.stderr
    );
    assert!(
        !mcp_requests(&stub).is_empty(),
        "the selected non-loopback HTTP MCP server was not contacted"
    );
}

#[test]
fn offline_stdio_refuses_before_the_mcp_child_can_spawn() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let marker = ego.workspace_path().join("stdio-spawned");
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.local]\ntransport = 'stdio'\ncommand = '/bin/sh'\nargs = ['-c', 'printf spawned > {}']\n",
            marker.display()
        ),
    );

    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            FIXTURE_SESSION_ID,
            "--output",
            "jsonl",
            "--offline",
            "--mcp",
            "local",
            "Ask",
        ])
        .run();

    assert_ne!(finished.code, Some(0), "offline stdio run was admitted");
    assert!(!marker.exists(), "offline refusal spawned the MCP child");
    assert_no_durable_run_admission(&finished);
}

#[test]
fn allowed_http_run_and_resume_discover_before_each_selected_execution() {
    let stub = Stub::start_mcp(Plan {
        completions: vec![support::Reply::Sse(text("done"))],
        ..Plan::default()
    });
    let places = support::Places::new();
    places
        .ego(&stub)
        .home_file("config.toml", &http_config(&stub.address));

    let first = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--output",
            "jsonl",
            "--sandbox",
            "off",
            "--mcp",
            "remote",
            "first",
        ])
        .run();
    assert_eq!(first.code, Some(0), "allowed run failed: {}", first.stderr);
    let after_run = mcp_requests(&stub).len();
    assert!(after_run > 0, "allowed run skipped HTTP MCP discovery");
    assert!(
        stub.generations().iter().any(|_| true),
        "allowed run skipped selected provider execution"
    );

    let second = places
        .ego(&stub)
        .subcommand("resume")
        .args([
            "--model",
            "ollama/qwen3",
            "--output",
            "jsonl",
            "--sandbox",
            "off",
            "--mcp",
            "remote",
            "second",
        ])
        .run();
    assert_eq!(
        second.code,
        Some(0),
        "allowed resume failed: {}",
        second.stderr
    );
    assert!(
        mcp_requests(&stub).len() > after_run,
        "resume skipped HTTP MCP discovery"
    );
    assert!(
        stub.generations().len() >= 2,
        "run and resume did not both use the selected execution path"
    );
}

#[test]
fn explicit_mcp_management_keeps_its_declared_online_authority() {
    let stub = Stub::start_mcp(Plan::default());
    let ego = Ego::against(&stub);
    ego.home_file("config.toml", &http_config(&stub.address));

    let listed = ego.subcommand("mcp").args(["ls", "--json"]).run();
    assert_eq!(listed.code, Some(0), "mcp ls failed: {}", listed.stderr);
    assert!(
        !mcp_requests(&stub).is_empty(),
        "explicit MCP management did not use its declared online authority"
    );
    assert!(
        stub.generations().is_empty(),
        "MCP management unexpectedly entered provider execution"
    );
}
