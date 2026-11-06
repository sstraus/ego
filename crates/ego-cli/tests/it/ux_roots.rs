use crate::support;
use support::{Ego, Plan, Stub};

// Catches: roots access values are discoverable only through a usage failure.
#[test]
fn roots_add_help_lists_supported_access_values() {
    let result = Ego::nowhere()
        .subcommand("roots")
        .args(["add", "--help"])
        .run();
    assert_eq!(result.code, Some(0));
    assert!(result.stdout.contains("read-write"), "{}", result.stdout);
}

// Catches: the roots CLI exposes transaction jargon and raw arrays to a human.
#[test]
fn roots_text_uses_a_table_and_plain_receipt_without_changing_json() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    let path = places.workspace_path().display().to_string();
    let empty = places.ego(&stub).subcommand("roots").args(["ls"]).run();
    assert!(
        empty.stdout.contains("No roots are stored"),
        "{}",
        empty.stdout
    );
    let added = places
        .ego(&stub)
        .subcommand("roots")
        .args(["add", "--access", "read-write", &path])
        .run();
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    assert!(added.stdout.contains("Saved root"), "{}", added.stdout);
    assert!(
        !added.stdout.contains("published_durably"),
        "{}",
        added.stdout
    );
    let listed = places.ego(&stub).subcommand("roots").args(["ls"]).run();
    assert!(
        listed.stdout.starts_with("ACCESS\tPATH\n"),
        "{}",
        listed.stdout
    );
    assert!(
        listed.stdout.contains(&format!("read-write\t{path}")),
        "{}",
        listed.stdout
    );
    let json = places
        .ego(&stub)
        .subcommand("roots")
        .args(["ls", "--json"])
        .run();
    let json: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(json["values"]["roots"][0]["access"], "read-write");
}
