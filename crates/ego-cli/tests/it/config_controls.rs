//! Behavioral contract for CLI configuration and control surfaces.

use std::process::Command;

fn ego(args: &[&str]) -> std::process::Output {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_ego"))
        .env_clear()
        .env("HOME", home.path())
        .env("EGO_HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .current_dir(workspace.path())
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn config_get_and_set_are_real_atomic_management_commands() {
    let missing = ego(&["config", "get", "model"]);
    assert_eq!(
        missing.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    assert!(missing.stdout.is_empty());
    let set = ego(&["config", "set", "model=\"provider/model\""]);
    assert_eq!(
        set.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&set.stdout).lines().count(), 1);
}

#[test]
fn config_set_persists_yolo_mode() {
    let result = ego(&["config", "set", "mode=\"yolo\""]);
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn managed_config_rejects_permission_rule_mutation() {
    let invalid = ego(&["config", "set", "permissions.allow=['read(src/**)']"]);
    assert_eq!(invalid.status.code(), Some(2));
}

#[test]
fn a_single_secret_assignment_failure_has_no_receipt_or_secret_leak() {
    let result = ego(&["config", "set", "model=SECRET-DO-NOT-LEAK"]);
    assert_eq!(result.status.code(), Some(2));
    assert!(result.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("SECRET-DO-NOT-LEAK"));
}

#[test]
fn editor_management_is_a_process_boundary_with_actionable_missing_editor() {
    let result = ego(&["config", "edit"]);
    assert_eq!(
        result.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("EDITOR"));
}
