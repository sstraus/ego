use super::*;

// Catches: a busy MCP listing leaks Rust structs, revisions and timestamps into the composer.
#[test]
fn busy_mcp_inventory_is_user_facing_and_bounded() {
    let (reply, gate) = gated_paced_line("MCP-538-RUNNING");
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("AGENTS.md", "# MCP inventory fixture\n");
    let server = ego.file("inventory-server.py", SESSION_MCP_SERVER);
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.auto]\ntransport='stdio'\ncommand='/usr/bin/python3'\nargs=[{:?}]\n",
            server.display().to_string()
        ),
    );
    let (session, mut input, reader) = PtySession::start(&ego, 24, 80, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").expect("trust");
    input.flush().expect("flush");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"hold stream\r").expect("turn");
    input.flush().expect("flush");
    until(&reader, &mut output, b"MCP-538-RUNNING");
    let start = output.len();
    input.write_all(b"/mcp\r").expect("inventory");
    input.flush().expect("flush");
    until_notice_prose(
        &reader,
        &mut output,
        start,
        "/mcp login NAME · /mcp toggle NAME · /mcp add NAME …",
    );
    record_slash_capture(&output, 24, 80);
    let listing = normalized(&output[start..]);
    gate.release();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("flush");
    session.quit(input);
    assert!(
        !listing.contains("McpServerStatus"),
        "debug inventory: {listing}"
    );
    assert!(
        !listing.contains("Instant") && !listing.contains("Some("),
        "debug inventory: {listing}"
    );
    assert!(
        listing.contains("auto") && listing.contains("admitted"),
        "missing server: {listing}"
    );
    assert!(
        listing.contains("/mcp toggle") && listing.contains("/mcp add"),
        "missing syntax: {listing}"
    );
    assert_eq!(stub.generations().len(), 1);
}

// Catches: /tools details reads only the local registry and omits admitted MCP tools.
#[test]
fn tools_details_lists_admitted_mcp_tools() {
    let stub = Stub::start(Plan::default());
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("AGENTS.md", "# MCP tools fixture\n");
    let server = ego.file("tools-server.py", SESSION_MCP_SERVER);
    ego.home_file(
        "config.toml",
        &format!(
            "[mcp.servers.auto]\ntransport='stdio'\ncommand='/usr/bin/python3'\nargs=[{:?}]\n",
            server.display().to_string()
        ),
    );
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust");
    input.flush().expect("flush");
    until(&reader, &mut output, b"/help");
    let list_start = output.len();
    input.write_all(b"/mcp\r").expect("list");
    input.flush().expect("flush");
    until_normalized(
        &reader,
        &mut output,
        list_start,
        "Choose MCP server or action",
    );
    input.write_all(b"\r").expect("show server status");
    input.flush().expect("flush");
    until_normalized(&reader, &mut output, list_start, "admitted");
    let start = output.len();
    input.write_all(b"/tools details\r").expect("tools");
    input.flush().expect("flush");
    until_local_action_completed(&reader, &mut output, start, "tools", SLASH_ACTION_DEADLINE);
    drain_until_quiet(&reader, &mut output);
    let visible = normalized(&output[start..]);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("flush");
    assert!(session.quit(input).success());
    assert!(
        visible.contains("search_tools"),
        "MCP tool missing: {visible}"
    );
    assert!(
        visible.contains("read_tool_result"),
        "builtin missing: {visible}"
    );
    assert!(stub.generations().is_empty());
}
