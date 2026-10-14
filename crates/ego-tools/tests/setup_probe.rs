//! Real filesystem setup probes do not perform intended work.

use ego_core::config::{Mode, PermCfg, Ruleset, Source};
use ego_core::permit::{CanonPath, PolicyEngine};
use ego_core::setup::{Controller, DiagnosticScope, Probe, ProbeReader};
use ego_tools::setup::NativeProbeReader;

// Catches: diagnostic reach is confused with normal read-tool authority.
#[test]
fn setup_diagnostic_read_observes_without_widening_target_policy() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let file = root.path().join("instructions.md");
    std::fs::write(&file, "# verified fixture\n").unwrap();
    let canonical = CanonPath::new(std::fs::canonicalize(&file).unwrap());
    let reader = NativeProbeReader::capture();
    let policy = PolicyEngine::new(
        CanonPath::new(&workspace),
        Mode::Default,
        Ruleset::from_layer(PermCfg::default(), Source::User),
    );
    let scope = DiagnosticScope::new(vec![canonical]);
    let controller = Controller::new(&reader, &policy, &scope);
    let report = controller
        .inspect(&[Probe::Read {
            path: file.clone(),
            reason: "instruction dependency".into(),
        }])
        .unwrap();
    assert!(report[0].observed);
    assert!(matches!(report[0].status, ego_core::setup::Status::Blocked));
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        "# verified fixture\n"
    );
}

// Catches: a setup read waits on a FIFO or reads unbounded regular-file contents.
#[test]
fn setup_native_read_bounds_bytes_and_refuses_nonregular_files() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("large");
    std::fs::write(&file, [b'x'; 100]).unwrap();
    let reader = NativeProbeReader::capture();
    let result = reader.read(&CanonPath::new(&file), 10).unwrap();
    assert_eq!(result.bytes.len(), 10);
    assert!(result.truncated);
    assert!(reader.read(&CanonPath::new(root.path()), 10).is_err());
}

// Catches: a regular-file alias turns a diagnostic scope into a credential read.
#[test]
fn setup_instruction_alias_to_credential_is_blocked_before_content_read() {
    let root = tempfile::tempdir().unwrap();
    let secret_dir = root.path().join(".ssh");
    std::fs::create_dir(&secret_dir).unwrap();
    let secret = secret_dir.join("id_test");
    std::fs::write(&secret, "synthetic never-read bytes").unwrap();
    let alias = root.path().join("AGENTS.md");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, &alias).unwrap();
    #[cfg(not(unix))]
    return;
    let reader = NativeProbeReader::capture();
    let policy = PolicyEngine::new(CanonPath::new(root.path()), Mode::Yolo, Ruleset::default());
    let scope = DiagnosticScope::new(vec![CanonPath::new(
        std::fs::canonicalize(&secret).unwrap(),
    )]);
    let report = Controller::new(&reader, &policy, &scope)
        .inspect(&[Probe::Instructions {
            path: alias,
            reason: "malicious fixture reference".into(),
        }])
        .unwrap();
    assert!(matches!(report[0].status, ego_core::setup::Status::Blocked));
    assert!(!report[0].observed);
}
