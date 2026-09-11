//! contract for the headless `ego compact` host surface.

use ego_cli::cli::{CompactArgs, Invocation, Output, parse};

const SOURCE: &str = "00000000-0000-0000-0000-000000000068";
const REQUEST: &str = "00000000-0000-0000-0000-000000000069";

fn compact(args: &[&str]) -> Result<CompactArgs, String> {
    match parse(args.iter().map(|value| (*value).to_owned()).collect()) {
        Ok(Invocation::Compact(args)) => Ok(*args),
        Ok(other) => Err(format!("expected compact invocation, got {other:?}")),
        Err(error) => Err(error.to_string()),
    }
}

#[test]
fn compact_requires_a_source_and_explicit_request_id() {
    let args = compact(&["ego", "compact", SOURCE, "--request-id", REQUEST])
        .expect("valid compact invocation");
    assert_eq!(args.source.to_string(), SOURCE);
    assert_eq!(args.request_id.uuid().to_string(), REQUEST);
}

#[test]
fn compact_preserves_output_and_root_selection_flags() {
    let args = compact(&[
        "ego",
        "compact",
        SOURCE,
        "--request-id",
        REQUEST,
        "--output",
        "jsonl",
        "-C",
        "/workspace",
        "--session-dir",
        "/sessions",
    ])
    .expect("valid compact invocation");
    assert_eq!(args.output, Output::Jsonl);
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
fn compact_refuses_an_implicit_request_id() {
    let result = parse(
        ["ego", "compact", SOURCE]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    );
    assert!(result.is_err());
}

#[test]
fn compact_has_no_alias_or_run_command_mode() {
    for alias in ["--compact", "compress"] {
        let result = parse(
            ["ego", alias, SOURCE, "--request-id", REQUEST]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );
        assert!(
            result.is_err(),
            "unsupported compact alias {alias} was accepted"
        );
    }
}

#[test]
fn compact_requires_a_real_session_id_and_request_id() {
    for value in ["not-a-session", "00000000-0000-0000-0000-000000000000"] {
        let result = parse(
            ["ego", "compact", value, "--request-id", REQUEST]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );
        assert!(result.is_err(), "invalid source {value} was accepted");
    }
}
