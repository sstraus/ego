//! Goal evaluator argv must cross clap as one typed value, not a collection.

mod support;

use ego_cli::cli::{self, Invocation};

fn parse(args: &[&str]) -> Result<Invocation, cli::UsageError> {
    cli::parse(
        std::iter::once("ego")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect(),
    )
}

/// Catches: clap declares Vec<String> values but tries to extract String elements.
#[test]
fn goal_command_json_argv_does_not_panic_on_any_run_surface() {
    for prefix in [
        vec!["run"],
        vec!["resume", support::FIXTURE_SESSION_ID],
        vec!["continue", support::FIXTURE_SESSION_ID],
        vec![],
    ] {
        let mut argv = prefix;
        argv.extend([
            "--goal-check-command",
            r#"["checker","argument with spaces",""]"#,
            "--goal-turns",
            "3",
            "Check it",
        ]);
        let invocation = parse(&argv).expect("valid evaluator argv parses");
        let args = match invocation {
            Invocation::Run(args) | Invocation::Resume(args) | Invocation::Interactive(args) => {
                args
            }
            other => panic!("unexpected invocation: {other:?}"),
        };
        assert_eq!(args.goal_turns, 3);
        assert_eq!(
            args.goal_check_command,
            Some(vec![
                "checker".into(),
                "argument with spaces".into(),
                "".into()
            ])
        );
    }
}

/// Catches: accepting an unusable executable or non-array command through clap.
#[test]
fn goal_command_rejects_invalid_json_and_empty_executables() {
    for value in ["not json", r#""checker""#, "[]", r#"["  "]"#, "[1]"] {
        assert!(parse(&["run", "--goal-check-command", value, "Check it"]).is_err());
    }
}

/// Catches: scalar argv silently accumulating repeated flags or leaking into ACP.
#[test]
fn goal_command_is_a_single_run_only_value() {
    assert!(
        parse(&[
            "run",
            "--goal-check-command",
            r#"["one"]"#,
            "--goal-check-command",
            r#"["two"]"#
        ])
        .is_err()
    );
    assert!(parse(&["acp", "--goal-check-command", r#"["one"]"#]).is_err());
    let Invocation::Run(args) = parse(&["run", "--goal", "Checked objective", "Check it"]).unwrap()
    else {
        panic!("expected run");
    };
    assert_eq!(args.goal.as_deref(), Some("Checked objective"));
    assert_eq!(args.goal_turns, 20);
    assert_eq!(args.goal_check_command, None);
}

/// Catches: the real binary panics before admission instead of invoking the evaluator.
#[cfg(unix)]
#[test]
fn goal_command_binary_invokes_evaluator_and_persists_achievement() {
    use support::{Places, Plan, Reply, Stub, text};
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("The requested work is complete."))],
        ..Plan::default()
    });
    let evidence = places.workspace_path().join("evaluator-input.json");
    let script = places.file("check.sh", "cat > \"$1\"\nprintf '%s\\n' '{\"achieved\":true,\"reason\":\"script verified the work\"}'\n");
    let command = serde_json::json!(["/bin/sh", script, evidence]).to_string();
    let finished = places
        .ego(&stub)
        .args([
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--goal",
            "Checked objective",
            "--goal-check-command",
            &command,
            "--allow",
            "exec(/bin/sh *)",
            "Finish the work",
        ])
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let input: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(evidence).expect("evaluator ran")).unwrap();
    assert_eq!(input["objective"], "Checked objective");
    assert!(
        input["session"]
            .to_string()
            .contains("The requested work is complete.")
    );
    assert_eq!(stub.generations().len(), 1);
    let metadata = support::Ego::at(places.home_path(), places.workspace_path(), None)
        .subcommand("resume")
        .args([
            support::FIXTURE_SESSION_ID,
            "--goal",
            "show",
            "--output",
            "json",
        ])
        .run();
    assert_eq!(metadata.code, Some(0), "{}", metadata.stderr);
    assert_eq!(metadata.report()["goal"]["state"], "achieved");
    assert_eq!(metadata.report()["goal"]["achieved_by"], "check");
}
