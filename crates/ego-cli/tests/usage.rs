//! contract for regression case: one versioned usage report over one ledger.

use ego_core::usage::USAGE_REPORT_VERSION;
use serde_json::Value;

mod support;

use support::{Ego, Places, Plan, Stub, text};

fn usage<const N: usize>(args: [&str; N]) -> support::Finished {
    Ego::nowhere().subcommand("usage").args(args).run()
}

fn seeded_usage<const N: usize>(args: [&str; N]) -> support::Finished {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![support::Reply::Sse(text("usage fixture"))],
        ..Plan::default()
    });
    let run = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
            "Seed usage",
        ])
        .run();
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    places.ego(&stub).subcommand("usage").args(args).run()
}

fn json_report(finished: &support::Finished) -> Value {
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    finished.report()
}

#[test]
fn usage_json_is_a_versioned_report_with_all_seven_role_lines_and_total() {
    let report = json_report(&seeded_usage(["--json"]));

    assert_eq!(report["v"], USAGE_REPORT_VERSION);
    assert!(report["as_of"].is_number(), "{report}");
    assert_eq!(report["period"], serde_json::json!({"kind": "all"}));
    let sessions = report["sessions"].as_array().expect("report sessions");
    assert_eq!(sessions.len(), 1);
    let lines = sessions[0]["lines"].as_object().expect("report lines");
    assert_eq!(lines.len(), 7);
    for role in [
        "main",
        "judge",
        "goal_check",
        "compaction",
        "subagent",
        "hook",
        "total",
    ] {
        assert!(lines.contains_key(role), "missing {role} line: {lines:?}");
    }
    assert!(report["totals"].is_object(), "workspace totals are present");
}

#[test]
fn usage_accepts_only_all_or_positive_seconds_minutes_and_hours() {
    for period in ["all", "1s", "2m", "3h"] {
        let finished = seeded_usage(["--period", period, "--json"]);
        assert_eq!(
            finished.code,
            Some(0),
            "accepted period {period:?}: {}",
            finished.stderr
        );
    }
    for period in ["0s", "0m", "0h", "1d", "-1m", "1.5h", "2026-08-30"] {
        let finished = usage(["--period", period, "--json"]);
        assert_eq!(
            finished.code,
            Some(2),
            "invalid period {period:?} was accepted: {}",
            finished.stdout
        );
        assert!(finished.stderr.contains(period), "{}", finished.stderr);
    }
}

#[test]
fn usage_text_and_json_are_deterministic_projections_of_the_same_report() {
    let text = seeded_usage([]);
    let json = seeded_usage(["--json"]);
    assert_eq!(text.code, Some(0), "stderr: {}", text.stderr);
    let report = json_report(&json);
    // The table is the report read out loud, not a second report: every role
    // JSON carries is reachable in text, and JSON keeps all seven either way.
    // regression case hid the silent ones behind `--all-roles`, because a fixture
    // that only ever ran a main turn should not print five empty rows.
    let all_roles = seeded_usage(["--all-roles"]);
    assert_eq!(all_roles.code, Some(0), "stderr: {}", all_roles.stderr);
    for role in [
        "main",
        "judge",
        "goal_check",
        "compaction",
        "subagent",
        "hook",
        "total",
    ] {
        assert!(all_roles.stdout.contains(role), "{}", all_roles.stdout);
    }
    for role in ["main", "total"] {
        assert!(text.stdout.contains(role), "{}", text.stdout);
    }
    for silent in ["judge", "compaction", "subagent", "hook"] {
        assert!(
            !text.stdout.contains(silent),
            "a role that did nothing still printed a row: {}",
            text.stdout
        );
    }
    assert!(report["totals"]["main"].is_object(), "{report}");
    assert!(
        text.stderr.is_empty(),
        "diagnostics leaked to stdout: {}",
        text.stderr
    );
}

#[test]
fn usage_reports_unknown_cost_and_keeps_currencies_separate() {
    let report = json_report(&seeded_usage(["--json"]));
    let totals = &report["totals"];
    assert!(totals["main"]["cost"]["known"].is_array(), "{report}");
    assert!(totals["main"]["cost"]["unknown"].is_array(), "{report}");
    assert!(report["sessions"][0].get("lineage").is_none());
}

#[test]
fn usage_context_occupancy_has_session_model_and_provenance_without_billing_totals() {
    let report = json_report(&seeded_usage(["--period", "1h", "--json"]));
    for session in report["sessions"].as_array().expect("session entries") {
        assert!(session["session"].is_string(), "{session}");
        assert!(session["model"].is_string(), "{session}");
        let context = session["context"].as_object().expect("context entry");
        assert!(context["provenance"].is_string(), "{context:?}");
        assert!(session["lines"].get("context").is_none());
    }
}
