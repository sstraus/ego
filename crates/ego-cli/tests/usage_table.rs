//! contract for regression case: `ego usage` reads as a table, not a dump.
//!
//! The versioned report is the machine contract and does not move. What moves
//! is the human view: five zero role lines per session, each a `key=value`
//! run-on, buried the one line anybody wanted. The text view therefore shows
//! the columns a person asked for and drops the roles that did nothing, while
//! `--all-roles` restores the complete breakdown.

use serde_json::Value;

mod support;

use support::{Places, Plan, Stub};

/// A workspace with exactly one session that spent tokens on `main` only.
fn seeded<const N: usize>(args: [&str; N]) -> support::Finished {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![support::Reply::Sse(support::costing(
            "usage fixture",
            11,
            7,
        ))],
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

const IDLE_ROLES: [&str; 4] = ["judge", "compaction", "subagent", "hook"];

#[test]
fn usage_text_is_a_table_with_the_six_asked_for_columns_plus_role() {
    let listed = seeded([]);
    assert_eq!(listed.code, Some(0), "stderr: {}", listed.stderr);
    let header = listed
        .stdout
        .lines()
        .find(|line| line.starts_with("session"))
        .unwrap_or_else(|| panic!("no header row in:\n{}", listed.stdout));
    let columns: Vec<&str> = header.split_whitespace().collect();
    assert_eq!(
        columns,
        ["session", "model", "role", "in", "out", "cached", "cost"],
        "the table names exactly the columns the report shows"
    );
    assert!(
        !listed.stdout.contains("uncached_input="),
        "the key=value dump is gone:\n{}",
        listed.stdout
    );
}

#[test]
fn usage_text_hides_roles_that_did_nothing() {
    let listed = seeded([]);
    assert_eq!(listed.code, Some(0), "stderr: {}", listed.stderr);
    assert!(
        listed.stdout.contains("main"),
        "the role that spent tokens is shown:\n{}",
        listed.stdout
    );
    for role in IDLE_ROLES {
        assert!(
            !listed.stdout.contains(role),
            "idle role `{role}` is hidden:\n{}",
            listed.stdout
        );
    }
    assert!(
        listed.stdout.contains("workspace"),
        "the workspace total is always shown:\n{}",
        listed.stdout
    );
}

#[test]
fn all_roles_restores_the_complete_breakdown() {
    let listed = seeded(["--all-roles"]);
    assert_eq!(listed.code, Some(0), "stderr: {}", listed.stderr);
    for role in IDLE_ROLES {
        assert!(
            listed.stdout.contains(role),
            "`--all-roles` shows `{role}`:\n{}",
            listed.stdout
        );
    }
}

#[test]
fn all_roles_does_not_change_the_json_report() {
    let plain = seeded(["--json"]);
    let widened = seeded(["--all-roles", "--json"]);
    assert_eq!(plain.code, Some(0), "stderr: {}", plain.stderr);
    assert_eq!(widened.code, Some(0), "stderr: {}", widened.stderr);
    let plain: Value = serde_json::from_str(plain.stdout.trim()).expect("one report object");
    let widened: Value = serde_json::from_str(widened.stdout.trim()).expect("one report object");
    let lines = plain["sessions"][0]["lines"]
        .as_object()
        .expect("report lines");
    assert_eq!(lines.len(), 6, "every role line survives in JSON");
    assert_eq!(
        plain["sessions"], widened["sessions"],
        "the machine contract does not read a presentation flag"
    );
}
