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
    let script = places.file("check.sh", "cat > \"$1\"\nprintf '%s\\n' '{\"verdict\":\"achieved\",\"reason\":\"script verified the work\"}'\n");
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
            "--goal-operation",
            "show",
            "--output",
            "json",
        ])
        .run();
    assert_eq!(metadata.code, Some(0), "{}", metadata.stderr);
    assert_eq!(metadata.report()["goal"]["state"], "achieved");
    assert_eq!(metadata.report()["goal"]["achieved_by"], "check");
}

/// Catches: confinement drops the evidence pipe, or stderr contaminates the verdict.
#[cfg(unix)]
#[test]
fn goal_command_workspace_child_reads_evidence_and_ignores_stderr() {
    use support::{Places, Plan, Reply, Stub, text};
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("verified artifact"))],
        ..Plan::default()
    });
    let input = places.workspace_path().join("evidence.json");
    let script = places.file("check.sh", "cat > \"$1\"\nprintf 'diagnostic noise\\n' >&2\nprintf '%s\\n' '{\"verdict\":\"achieved\",\"reason\":\"verified\"}'\n");
    let command = serde_json::json!(["/bin/sh", script, input]).to_string();
    let finished = places
        .ego(&stub)
        .args([
            "--sandbox",
            "workspace",
            "--model",
            "ollama/qwen3",
            "--output",
            "jsonl",
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
    let evidence: serde_json::Value =
        serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
    assert_eq!(evidence["objective"], "Checked objective");
    assert!(
        evidence["session"]
            .to_string()
            .contains("verified artifact")
    );
    assert!(
        goal_events(&places)
            .iter()
            .any(|e| e["event"]["kind"]["transition"]["kind"] == "achieved"),
        "{}",
        finished.stdout
    );
    assert!(!finished.stdout.contains("diagnostic noise"));
    assert_eq!(stub.generations().len(), 1);
}

/// Catches: the real child's failure streams are lost when ProcessError is rendered.
#[cfg(unix)]
#[test]
fn goal_command_failed_child_reports_bounded_stderr_tail() {
    use support::{Places, Plan, Reply, Stub, text};
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("finished"))],
        ..Plan::default()
    });
    let script = places.file("fail.sh", "cat >/dev/null\nprintf 'discard-this-prefix\\n' >&2\ni=0; while [ \"$i\" -lt 5000 ]; do printf x >&2; i=$((i+1)); done\nprintf '\\nprecise failure tail\\n' >&2\nexit 7\n");
    let command = serde_json::json!(["/bin/sh", script]).to_string();
    let finished = places
        .ego(&stub)
        .args([
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
            "--output",
            "jsonl",
            "--goal",
            "Checked objective",
            "--goal-check-command",
            &command,
            "--allow",
            "exec(/bin/sh *)",
            "Finish the work",
        ])
        .run();
    let payloads = goal_events(&places);
    let stopped = payloads
        .iter()
        .map(|e| &e["event"]["kind"])
        .find(|e| e["kind"] == "GoalCheckStopped")
        .expect("goal check stopped");
    let reason = stopped["reason"].as_str().unwrap();
    assert!(reason.contains("precise failure tail"), "{reason}");
    assert!(reason.contains("Some(7)"), "{reason}");
    assert!(!reason.contains("discard-this-prefix"), "{reason}");
    assert!(reason.len() < 4300, "{}", reason.len());
    assert!(
        finished.stdout.contains("precise failure tail"),
        "{}",
        finished.stdout
    );
}

#[cfg(unix)]
fn goal_events(places: &support::Places) -> Vec<serde_json::Value> {
    std::fs::read_dir(places.home_path().join("sessions"))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|e| std::fs::read_to_string(e.path().join("events.jsonl")).ok())
        .flat_map(|text| {
            text.lines()
                .map(|line| {
                    serde_json::from_str::<serde_json::Value>(line).unwrap()["payload"].clone()
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Catches: an invalid real-child verdict is accepted as achievement.
#[cfg(unix)]
#[test]
fn goal_command_invalid_child_verdict_never_achieves() {
    use support::{Places, Plan, Reply, Stub, text};
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("finished"))],
        ..Plan::default()
    });
    let command =
        serde_json::json!(["/bin/sh", "-c", "cat >/dev/null; printf 'not a verdict'"]).to_string();
    let finished = places
        .ego(&stub)
        .args([
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
            "--output",
            "jsonl",
            "--goal",
            "Checked objective",
            "--goal-check-command",
            &command,
            "--allow",
            "exec(/bin/sh *)",
            "Finish the work",
        ])
        .run();
    assert!(
        finished.stdout.contains("invalid verdict"),
        "{}",
        finished.stdout
    );
    assert!(
        !goal_events(&places)
            .iter()
            .any(|e| e["event"]["kind"]["transition"]["kind"] == "achieved")
    );
    assert_eq!(stub.generations().len(), 1);
}

/// Catches: a failed evaluator looks like an achieved goal in the JSON summary.
#[cfg(unix)]
#[test]
fn goal_json_summary_reports_stopped_goal_with_reason() {
    let report = goal_json_summary("printf 'precise failure tail\\n' >&2; exit 7", 0);
    assert_eq!(report["goal"]["status"], "stopped");
    let reason = report["goal"]["reason"].as_str().unwrap();
    assert!(reason.contains("precise failure tail"), "{reason}");
    assert!(reason.contains("Some(7)"), "{reason}");
}

/// Catches: achieved goals are indistinguishable from runs without a goal.
#[cfg(unix)]
#[test]
fn goal_json_summary_reports_achieved_goal() {
    let report = goal_json_summary(
        r#"printf '%s\n' '{"verdict":"achieved","reason":"verified artifact"}'"#,
        0,
    );
    assert_eq!(report["goal"]["status"], "achieved");
    assert!(report["goal"].get("reason").is_none());
}

/// Catches: exhausted continuation allowance is reported as achieved or generic failure.
#[cfg(unix)]
#[test]
fn goal_json_summary_reports_exhausted_goal() {
    let report = goal_json_summary(
        r#"printf '%s\n' '{"verdict":"continue","reason":"artifact missing"}'"#,
        0,
    );
    assert_eq!(report["goal"]["status"], "exhausted");
    assert_eq!(
        report["goal"]["reason"],
        "goal continuation budget exhausted (0)"
    );
}

#[cfg(unix)]
fn goal_json_summary(verdict: &str, continuation_turns: u32) -> serde_json::Value {
    use support::{Places, Plan, Reply, Stub, text};
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("finished"))],
        ..Plan::default()
    });
    let script = places.file("check.sh", &format!("cat >/dev/null\n{verdict}\n"));
    let command = serde_json::json!(["/bin/sh", script]).to_string();
    let finished = places
        .ego(&stub)
        .args([
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
            "--output",
            "json",
            "--goal",
            "Checked objective",
            "--goal-turns",
            &continuation_turns.to_string(),
            "--goal-check-command",
            &command,
            "--allow",
            "exec(/bin/sh *)",
            "Finish the work",
        ])
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    assert_eq!(stub.generations().len(), 1);
    let report = finished.report();
    assert_eq!(report["ok"], true);
    assert_eq!(report["exit_reason"], "done");
    report
}

/// Catches: quiet/headless runs hide goal progress or contaminate answer stdout.
#[cfg(unix)]
#[test]
fn goal_visibility_headless_quiet_keeps_receipts_on_stderr() {
    use support::{Places, Plan, Reply, Stub, text};
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("REGRESSION-MARKER")),
            Reply::Sse(text("REGRESSION-MARKER")),
        ],
        ..Plan::default()
    });
    let checker = serde_json::json!(["/bin/sh", "-c",
        "cat >/dev/null; printf '%s\\n' '{\"verdict\":\"continue\",\"reason\":\"artifact missing headless 527\"}'"
    ]).to_string();
    let finished = places
        .ego(&stub)
        .args([
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
            "--quiet",
            "--goal",
            "Verify headless artifact 527",
            "--goal-turns",
            "1",
            "--goal-check-command",
            &checker,
            "--allow",
            "exec(/bin/sh *)",
            "do the work",
        ])
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    assert!(
        finished.stdout.contains("REGRESSION-MARKER"),
        "{}",
        finished.stdout
    );
    assert!(
        finished.stdout.contains("REGRESSION-MARKER"),
        "{}",
        finished.stdout
    );
    assert!(!finished.stdout.contains("Goal"), "{}", finished.stdout);
    for receipt in [
        "Goal (operator): Verify headless artifact 527",
        "Goal turn 1 of 1: artifact missing headless 527",
        "Goal stopped: Verify headless artifact 527",
        "turns 1/1",
        "/goal show",
        "/goal achieve",
        "/goal block",
        "--goal-turns",
    ] {
        assert_eq!(
            finished.stderr.matches(receipt).count(),
            1,
            "missing or repeated {receipt}: {}",
            finished.stderr
        );
    }
    assert_eq!(stub.generations().len(), 2);
}
