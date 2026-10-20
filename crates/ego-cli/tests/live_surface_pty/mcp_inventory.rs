use super::*;

// Catches: a busy MCP listing leaks Rust structs, revisions and timestamps into the composer.
#[test]
fn busy_mcp_inventory_is_user_facing_and_bounded() {
    let (reply, gate) = gated_paced_line("REGRESSION-MARKER-RUNNING");
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
    until(&reader, &mut output, b"REGRESSION-MARKER-RUNNING");
    let start = output.len();
    input.write_all(b"/mcp\r").expect("inventory");
    input.flush().expect("flush");
    until_normalized(&reader, &mut output, start, "mcp:");
    drain_until_quiet(&reader, &mut output);
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
