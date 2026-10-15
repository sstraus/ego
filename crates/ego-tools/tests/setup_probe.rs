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

// Catches: symlink resolution stats a credential target before checking its floor.
#[test]
fn setup_alias_to_missing_credential_is_refused_before_target_metadata() {
    let root = tempfile::tempdir().unwrap();
    let secret = root.path().join(".ssh/id_missing");
    let alias = root.path().join("AGENTS.md");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, &alias).unwrap();
    #[cfg(not(unix))]
    return;
    let error = NativeProbeReader::capture().metadata(&alias).unwrap_err();
    assert_eq!(error, "credential diagnostic access is forbidden");
    assert!(!secret.exists());
}

// Catches: instruction references become shell code or an unbounded cycle.
#[test]
fn setup_inventory_follows_literal_reference_graph_with_cycle_bound() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("AGENTS.md"),
        "@child.md\n```sh\n@ignored.md\n```\n",
    )
    .unwrap();
    std::fs::write(root.path().join("child.md"), "@AGENTS.md\n").unwrap();
    let inventory = ego_tools::setup::inventory(root.path());
    assert!(inventory.iter().any(
        |probe| matches!(probe, Probe::Instructions { path, .. } if path.ends_with("child.md"))
    ));
    assert!(!inventory.iter().any(
        |probe| matches!(probe, Probe::Instructions { path, .. } if path.ends_with("ignored.md"))
    ));
    assert!(inventory.len() <= 64);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
}

// Catches: linked-worktree common-dir metadata is lost or queried via git execution.
#[test]
fn setup_inventory_observes_real_linked_git_layout_without_changing_it() {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    let linked = root.path().join("linked");
    let init = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .arg(&main)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let add = std::process::Command::new("git")
        .arg("-C")
        .arg(&main)
        .args(["worktree", "add", "--quiet", "--detach"])
        .arg(&linked)
        .output()
        .unwrap();
    // An unborn repository needs an initial fixture commit, not a fabricated selector.
    if !add.status.success() {
        let commit = std::process::Command::new("git")
            .arg("-C")
            .arg(&main)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "--quiet",
                "-m",
                "fixture",
            ])
            .output()
            .unwrap();
        assert!(
            commit.status.success(),
            "{}",
            String::from_utf8_lossy(&commit.stderr)
        );
        let add = std::process::Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["worktree", "add", "--quiet", "--detach"])
            .arg(&linked)
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "{}",
            String::from_utf8_lossy(&add.stderr)
        );
    }
    let before = std::fs::read(linked.join(".git")).unwrap();
    let inventory = ego_tools::setup::inventory(&linked);
    assert!(
        inventory
            .iter()
            .any(|probe| matches!(probe, Probe::Read { path, .. } if path.ends_with("commondir")))
    );
    assert!(
        inventory.iter().any(
            |probe| matches!(probe, Probe::Metadata { path, .. } if path.ends_with("objects"))
        )
    );
    assert_eq!(std::fs::read(linked.join(".git")).unwrap(), before);
}

// Catches: a missing ordinary output loses its existing-ancestor evidence.
#[test]
fn setup_absent_write_target_keeps_parent_identity_without_creating_it() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("new-output");
    let reader = NativeProbeReader::capture();
    let policy = PolicyEngine::new(
        CanonPath::new(root.path()),
        Mode::Default,
        Ruleset::default(),
    );
    let scope = DiagnosticScope::new(Vec::new());
    let report = Controller::new(&reader, &policy, &scope)
        .inspect(&[Probe::Write {
            path: target.clone(),
            reason: "expected output".into(),
        }])
        .unwrap();
    assert_eq!(
        report[0].canonical.as_deref(),
        Some(target.as_path()),
        "absent write canonical evidence missing"
    );
    assert!(matches!(report[0].status, ego_core::setup::Status::Blocked));
    assert!(!target.exists());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

// Catches: absent-target handling mistakes a dangling alias/credential/missing parent for a safe output.
#[test]
fn setup_absent_write_refuses_aliases_credentials_and_missing_parents() {
    let root = tempfile::tempdir().unwrap();
    let reader = NativeProbeReader::capture();
    assert!(
        reader
            .write_metadata(&root.path().join("missing/output"))
            .is_err()
    );
    assert_eq!(
        reader
            .write_metadata(&root.path().join(".ssh/id_missing"))
            .unwrap_err(),
        "credential diagnostic access is forbidden"
    );
    #[cfg(unix)]
    {
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(root.path().join("missing"), &alias).unwrap();
        assert!(reader.write_metadata(&alias).is_err());
    }
}

// Catches: setup omits operator-derived default cache sources and cannot explain deduplication.
#[test]
fn setup_inventory_names_default_cargo_cache_derivation_without_creating_it() {
    let root = tempfile::tempdir().unwrap();
    let probes = ego_tools::setup::inventory(root.path());
    assert!(probes.iter().any(|probe| matches!(probe, Probe::Metadata { reason, .. } if reason.contains("Cargo registry cache") && reason.contains("derived"))), "derived default cache provenance missing");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

// Catches: derived writable reach reopens Adaptive instruction/read-only floors.
#[test]
fn setup_adaptive_write_reach_subtracts_captured_floors() {
    use ego_core::permit::{
        AdaptivePerimeter, Confinement, ProfileRoot, RootAccess, RunConfinement,
    };
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let cache = root.path().join("cache");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&cache).unwrap();
    let policy = RunConfinement::new(Confinement::Adaptive, false, CanonPath::new(&workspace))
        .unwrap()
        .with_adaptive_perimeter(AdaptivePerimeter {
            home: CanonPath::new(root.path()),
            writable: vec![CanonPath::new(&cache)],
            credentials: vec![],
            controls: vec![],
        });
    assert!(
        ego_tools::exec::setup_write_reachable(&policy, &CanonPath::new(cache.join("output")))
            .unwrap()
    );
    assert!(
        !ego_tools::exec::setup_write_reachable(&policy, &CanonPath::new(cache.join("AGENTS.md")))
            .unwrap()
    );
    let readonly = policy.with_roots(vec![ProfileRoot::new(
        CanonPath::new(&cache),
        RootAccess::Read,
    )]);
    assert!(
        !ego_tools::exec::setup_write_reachable(&readonly, &CanonPath::new(cache.join("output")))
            .unwrap()
    );
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
}

// Catches: setup omits the native developer selector and operator mbx provenance.
#[test]
fn setup_inventory_names_developer_and_mbx_selection_without_execution() {
    let root = tempfile::tempdir().unwrap();
    let probes = ego_tools::setup::inventory(root.path());
    assert!(probes.iter().any(|probe| matches!(probe, Probe::Metadata { reason, .. } if reason.contains("mbx") && reason.contains("operator"))), "mbx catalogue source missing");
    #[cfg(target_os = "macos")]
    assert!(probes.iter().any(|probe| matches!(probe, Probe::Metadata { reason, .. } if reason.contains("developer selection"))));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

// Catches: tool presence or write policy is mislabeled as a successful execution/effect.
#[test]
fn setup_tool_presence_and_write_intent_never_claim_effect_success() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("target");
    std::fs::write(&file, "unchanged").unwrap();
    let reader = NativeProbeReader::capture();
    let policy = PolicyEngine::new(CanonPath::new(root.path()), Mode::Yolo, Ruleset::default());
    let scope = DiagnosticScope::new(Vec::new());
    let report = Controller::new(&reader, &policy, &scope)
        .inspect(&[
            Probe::Write {
                path: file.clone(),
                reason: "expected output".into(),
            },
            Probe::Tool {
                name: "git".into(),
                reason: "expected tool".into(),
            },
        ])
        .unwrap();
    assert!(matches!(report[0].status, ego_core::setup::Status::Unknown));
    assert!(matches!(report[1].status, ego_core::setup::Status::Unknown));
    assert_eq!(std::fs::read_to_string(file).unwrap(), "unchanged");
}

// Catches: the bounded instruction catalogue silently drops queued references.
#[test]
fn setup_inventory_reports_omitted_instruction_references() {
    let root = tempfile::tempdir().unwrap();
    let names = (0..16)
        .map(|id| format!("@branch-{id}.md\n"))
        .collect::<String>();
    std::fs::write(root.path().join("AGENTS.md"), names).unwrap();
    for id in 0..16 {
        std::fs::write(
            root.path().join(format!("branch-{id}.md")),
            format!("@leaf-{id}.md\n"),
        )
        .unwrap();
    }
    let inventory = ego_tools::setup::inventory_report(root.path(), &|_| true);
    assert!(
        inventory
            .omissions
            .iter()
            .any(|reason| reason.contains("instruction") && reason.contains("limit")),
        "bounded catalogue silently omitted queued references"
    );
    assert!(inventory.probes.len() <= 64);
}

// Catches: per-file reference and byte bounds disappear from catalogue evidence.
#[test]
fn setup_inventory_reports_reference_and_byte_limits() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("AGENTS.md"),
        (0..17)
            .map(|id| format!("@leaf-{id}.md\n"))
            .collect::<String>(),
    )
    .unwrap();
    let report = ego_tools::setup::inventory_report(root.path(), &|_| true);
    assert!(
        report
            .omissions
            .iter()
            .any(|reason| reason.contains("reference limit 16")
                && reason.contains("1 references omitted"))
    );
    std::fs::write(root.path().join("AGENTS.md"), vec![b'x'; 65537]).unwrap();
    let report = ego_tools::setup::inventory_report(root.path(), &|_| true);
    assert!(
        report
            .omissions
            .iter()
            .any(|reason| reason.contains("byte limit 65536"))
    );
}

// Catches: duplicate references consume bounded slots and lose their source chain.
#[test]
fn setup_inventory_deduplicates_same_kind_path_preserving_source_depths() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("AGENTS.md"), "@shared.md\n@branch.md\n").unwrap();
    std::fs::write(root.path().join("branch.md"), "@shared.md\n").unwrap();
    std::fs::write(root.path().join("shared.md"), "plain").unwrap();
    let report = ego_tools::setup::inventory_report(root.path(), &|_| true);
    let shared = report
        .probes
        .iter()
        .filter_map(|probe| match probe {
            Probe::Instructions { path, reason } if path == &root.path().join("shared.md") => {
                Some(reason)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(shared.len(), 1);
    assert!(shared[0].contains("depth 1") && shared[0].contains("depth 2"));
    assert!(report.omissions.is_empty());
}

// Catches: operator mbx configuration is ignored or its cache path is fabricated.
#[test]
fn setup_inventory_observes_recorded_operator_cache_source_without_creating_cache() {
    if std::env::var_os("EGO_SETUP_CATALOGUE_FIXTURE").is_none() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let config = home
            .path()
            .join("Library/Application Support/mbx/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        // Recorded from the operator mbx config on 2026-10-15; only the cache_dir field is relevant.
        std::fs::write(&config, r#"cache_dir = "/home/example/workspace""#).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "setup_inventory_observes_recorded_operator_cache_source_without_creating_cache",
            ])
            .env("EGO_SETUP_CATALOGUE_FIXTURE", "1")
            .env("HOME", home.path())
            .env("EGO_SETUP_CATALOGUE_WORKSPACE", workspace.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!home.path().join(".cargo").exists());
        assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 0);
        return;
    }
    let workspace =
        std::path::PathBuf::from(std::env::var_os("EGO_SETUP_CATALOGUE_WORKSPACE").unwrap());
    let probes = ego_tools::setup::inventory(&workspace);
    assert!(probes.iter().any(|probe| matches!(probe, Probe::Metadata { path, reason } if path == std::path::Path::new("/home/example/workspace") && reason.contains("bounded operator TOML"))));
    #[cfg(target_os = "macos")]
    assert!(probes.iter().any(|probe| matches!(probe, Probe::Metadata { reason, .. } if reason.contains("developer selection"))));
}

// Catches: action diagnostics launch commands or inspect credential aliases/unbounded argv.
#[test]
fn setup_action_is_metadata_only_and_rejects_invalid_targets() {
    use ego_core::setup::ProbeReader;
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("program");
    let marker = root.path().join("must-not-exist");
    std::fs::write(&program, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let reader = NativeProbeReader::capture();
    let argv = vec![program.to_string_lossy().into_owned(), "--expected".into()];
    let call = reader.action(&argv, root.path()).unwrap();
    assert!(
        matches!(call.ops(), [ego_core::permit::Op::Spawn { spec }] if spec.identity().is_none())
    );
    assert!(!marker.exists());
    assert!(reader.action(&[], root.path()).is_err());
    assert!(reader.action(&["relative".into()], root.path()).is_err());
    assert!(
        reader
            .action(&argv, std::path::Path::new("relative"))
            .is_err()
    );
    assert!(
        reader
            .action(&vec![argv[0].clone(); 65], root.path())
            .is_err()
    );
    assert!(
        reader
            .action(&[argv[0].clone(), "x".repeat(16 * 1024)], root.path())
            .is_err()
    );
    assert!(
        reader
            .action(&[argv[0].clone(), "invalid\0argument".into()], root.path())
            .is_err()
    );
    assert!(
        reader
            .action(&[root.path().display().to_string()], root.path())
            .is_err()
    );
    #[cfg(unix)]
    {
        let secret = root.path().join(".ssh");
        std::fs::create_dir(&secret).unwrap();
        let target = secret.join("id_rsa");
        std::fs::write(&target, "not observed").unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(target, &alias).unwrap();
        assert!(
            reader
                .action(&[alias.display().to_string()], root.path())
                .is_err()
        );
    }
    assert_eq!(
        std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() == "must-not-exist")
            .count(),
        0
    );
}
