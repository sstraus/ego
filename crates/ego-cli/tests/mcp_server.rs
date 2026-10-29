use std::num::NonZeroUsize;

use ego_cli::cli::{Invocation, parse};
use ego_core::config::ModelRef;
use ego_core::generator::types::ThinkingLevel;
use ego_core::run::RunSelections;

fn accepted(args: &[&str]) -> ego_cli::cli::McpServerArgs {
    match parse(args.iter().map(|value| (*value).to_owned()).collect()).expect("valid argv") {
        Invocation::McpServer(args) => *args,
        other => panic!("expected MCP server invocation, got {other:?}"),
    }
}

#[test]
fn mcp_server_requires_and_preserves_all_fixed_bounds() {
    let args = accepted(&[
        "ego",
        "mcp-server",
        "-C",
        "/workspace",
        "--session-dir",
        "/sessions",
        "--max-concurrent-runs",
        "2",
        "--retained-requests",
        "3",
        "--max-resource-bytes",
        "4096",
    ]);
    assert_eq!(args.max_concurrent_runs, NonZeroUsize::new(2).unwrap());
    assert_eq!(args.retained_requests, NonZeroUsize::new(3).unwrap());
    assert_eq!(args.max_resource_bytes, NonZeroUsize::new(4096).unwrap());
    assert_eq!(
        args.directory.as_deref(),
        Some(std::path::Path::new("/workspace"))
    );
    assert_eq!(
        args.session_dir.as_deref(),
        Some(std::path::Path::new("/sessions"))
    );
}

#[test]
fn mcp_server_rejects_missing_zero_and_output_flags() {
    let valid = [
        "ego",
        "mcp-server",
        "-C",
        "/workspace",
        "--session-dir",
        "/sessions",
        "--max-concurrent-runs",
        "2",
        "--retained-requests",
        "3",
        "--max-resource-bytes",
        "4096",
    ];
    for missing in [
        "--max-concurrent-runs",
        "--retained-requests",
        "--max-resource-bytes",
    ] {
        let mut args = valid.to_vec();
        let index = args.iter().position(|value| *value == missing).unwrap();
        args.drain(index..=index + 1);
        assert!(parse(args.into_iter().map(str::to_owned).collect()).is_err());
    }
    for zero in [
        "--max-concurrent-runs",
        "--retained-requests",
        "--max-resource-bytes",
    ] {
        let mut args = valid.to_vec();
        let index = args.iter().position(|value| *value == zero).unwrap() + 1;
        args[index] = "0";
        assert!(parse(args.into_iter().map(str::to_owned).collect()).is_err());
    }
    for suffix in [["--output", "json"], ["--json", ""]] {
        let mut args = valid.to_vec();
        args.push(suffix[0]);
        if !suffix[1].is_empty() {
            args.push(suffix[1]);
        }
        assert!(parse(args.into_iter().map(str::to_owned).collect()).is_err());
    }
}

#[test]
fn mcp_run_selection_preserves_absent_and_explicit_thinking_distinctly() {
    let absent = RunSelections::new(
        Some(ModelRef("openai/gpt-5.6-sol".to_owned())),
        None,
        None,
        None,
        None,
    );
    let absent_wire = serde_json::to_value(&absent).expect("absent selection serializes");
    assert_eq!(absent.thinking(), None);
    assert_eq!(absent_wire["thinking"], serde_json::Value::Null);
    assert_eq!(
        serde_json::from_value::<RunSelections>(absent_wire).expect("absent selection decodes"),
        absent
    );

    let explicit = RunSelections::new(
        Some(ModelRef("openai/gpt-5.6-sol".to_owned())),
        Some(ThinkingLevel::Off),
        None,
        None,
        None,
    );
    let explicit_wire = serde_json::to_value(&explicit).expect("explicit selection serializes");
    assert_eq!(explicit.thinking(), Some(ThinkingLevel::Off));
    assert_eq!(explicit_wire["thinking"], "off");
}
