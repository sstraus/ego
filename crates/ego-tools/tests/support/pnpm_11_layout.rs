use super::*;

// Catches: pnpm 11's engine store lock/database writes are omitted, or fixing
// them also opens the engine links that the unconfined host executes later.
#[test]
fn pnpm_11_engine_store_writes_keep_host_links_readonly() {
    let Some(root) =
        isolated("pnpm_11_layout::pnpm_11_engine_store_writes_keep_host_links_readonly")
    else {
        return;
    };
    if !reported_layout_child(
        &root,
        "pnpm_11_layout::pnpm_11_engine_store_writes_keep_host_links_readonly",
        false,
        false,
    ) {
        return;
    }
    // Layout recorded from pnpm@11.15.1 in the story's live scratch fixture.
    let store = root.join("home/Library/pnpm/package-manager-store/v11");
    let links = store.join("links/@/pnpm/11.15.1/bin");
    std::fs::create_dir_all(&links).unwrap();
    let engine = links.join("pnpm");
    std::fs::write(&engine, "host engine").unwrap();
    std::fs::create_dir_all(store.join("tmp/engine-locks")).unwrap();
    let policy = admit(&root.join("workspace")).unwrap();
    for path in [
        store.join("index.db"),
        store.join("tmp/engine-locks/package-manager-env.lock"),
    ] {
        let outcome = execute(
            policy.clone(),
            &root,
            vec!["/usr/bin/touch".into(), path.clone().into_os_string()],
        );
        assert!(
            outcome.is_ok() && path.exists(),
            "{}: {outcome:?}",
            path.display()
        );
    }
    let outcome = execute(
        policy,
        &root,
        vec!["/usr/bin/touch".into(), engine.clone().into_os_string()],
    );
    assert!(outcome.is_err(), "host engine must remain readonly");
    assert_eq!(std::fs::read_to_string(engine).unwrap(), "host engine");
}

// Catches: strict runtime roots admit Git's config but omit its default ignore.
#[test]
fn workspace_git_global_ignore_is_readable_without_opening_its_directory() {
    let Some(root) = isolated(
        "pnpm_11_layout::workspace_git_global_ignore_is_readable_without_opening_its_directory",
    ) else {
        return;
    };
    let git = root.join("home/.config/git");
    std::fs::create_dir_all(&git).unwrap();
    let ignore = git.join("ignore");
    std::fs::write(&ignore, "REGRESSION-MARKER").unwrap();
    let other = git.join("not-config");
    std::fs::write(&other, "PRIVATE-NEIGHBOR").unwrap();
    let config = root.join("home/.gitconfig");
    std::fs::write(&config, "[core]\nignorecase = false\n").unwrap();
    for level in [Confinement::Workspace, Confinement::Adaptive] {
        let policy =
            admit_confinement(level, false, CanonPath::new(root.join("workspace"))).unwrap();
        let read = execute(
            policy.clone(),
            &root,
            vec!["/bin/cat".into(), ignore.clone().into_os_string()],
        )
        .unwrap();
        assert!(
            read.content
                .iter()
                .any(|block| block.text.contains("REGRESSION-MARKER"))
        );
        execute(
            policy.clone(),
            &root,
            vec!["/bin/cat".into(), config.clone().into_os_string()],
        )
        .unwrap();
        if level == Confinement::Workspace {
            assert!(
                execute(
                    policy,
                    &root,
                    vec!["/bin/cat".into(), other.clone().into_os_string()]
                )
                .is_err()
            );
        }
    }
}

// Catches: admitting cache file paths still leaves the actual pnpm engine
// unable to open its operation lock. Requires the operator's cached pnpm 11.
#[test]
#[ignore = "live installed pnpm@11.15.1 prerequisite; run explicitly"]
fn pnpm_11_live_exec_uses_the_installed_engine_under_adaptive() {
    // A workspace cannot sit below the writable TMPDIR source: admission
    // correctly rejects a derived write root that contains the workspace.
    let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("package.json"),
        r#"{"name":"regression_case-live","private":true,"packageManager":"pnpm@11.15.1"}"#,
    )
    .unwrap();
    let policy = admit_confinement(
        Confinement::Adaptive,
        false,
        CanonPath::new(workspace.canonicalize().unwrap()),
    )
    .unwrap();
    let warnings = policy.admission_warnings().to_vec();
    let output = execute(
        policy,
        root.path(),
        vec![
            "/opt/homebrew/bin/pnpm".into(),
            "exec".into(),
            "/usr/bin/true".into(),
        ],
    );
    assert!(
        output.is_ok(),
        "real pnpm@11.15.1 must execute through the admitted perimeter: {output:?}; warnings: {warnings:?}"
    );
}

// This is a recorded-output adapter at the discovery boundary, not the live
// pnpm test below. Every policy assertion still executes real Seatbelt children.
fn reported_layout_child(
    root: &Path,
    name: &str,
    require_write: bool,
    require_scratch: bool,
) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    if std::env::var_os("EGO_PNPM_REPORTED_LAYOUT").is_some() {
        return true;
    }
    let bin = root.join("operator-bin");
    std::fs::create_dir(&bin).unwrap();
    let script = if require_write {
        "#!/bin/sh\nexec /usr/bin/touch \"$HOME/discovery-write\"\n".to_owned()
    } else {
        r#"#!/bin/sh
if [ "$1" = '--version' ]; then printf '%s\n' '11.15.1'; exit 0; fi
if [ "$1 $2" != 'with 11.15.1' ]; then exit 1; fi
shift 2
case "$1 $2" in
  'store path') printf '%s\n' "$HOME/Library/pnpm/store/v11" ;;
  'root -g') printf '%s\n' "$HOME/Library/pnpm/global/v11" ;;
  'bin -g') printf '%s\n' "$HOME/Library/pnpm/bin" ;;
  *) exit 1 ;;
esac
"#
        .to_owned()
    };
    let script = if require_scratch {
        script
            .replacen(
                "#!/bin/sh\n",
                "#!/bin/sh\ntouch \"$PWD/discovery-write\" || exit 1\n",
                1,
            )
            .replacen("'11.15.1'; exit", "'12.6.0'; exit", 1)
    } else {
        script
    };
    let script = if name.ends_with("::pnpm_11_discovery_bootstraps_only_the_selector_lock") {
        script.replacen(
            "#!/bin/sh\n",
            "#!/bin/sh\ntouch \"$HOME/Library/pnpm/package-manager-store/v11/tmp/engine-locks/package-manager-env.lock\" || exit 1\n",
            1,
        )
    } else {
        script
    };
    let program = bin.join("pnpm");
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join("workspace/package.json"),
        r#"{"packageManager":"pnpm@11.15.1"}"#,
    )
    .unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("EGO_PNPM_REPORTED_LAYOUT", "1")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .output()
        .unwrap();
    reexec::assert_one_child_test(&output);
    false
}

// Catches: an unavailable offline pin silently falls back to the host layout.
#[test]
fn pnpm_11_unavailable_pinned_version_warns_without_runtime_grants() {
    let name = "pnpm_11_layout::pnpm_11_unavailable_pinned_version_warns_without_runtime_grants";
    let Some(root) = isolated(name) else {
        return;
    };
    if !reported_layout_child(&root, name, false, false) {
        return;
    }
    std::fs::write(
        root.join("workspace/package.json"),
        r#"{"packageManager":"pnpm@10.33.1"}"#,
    )
    .unwrap();
    let engine = root.join("home/Library/pnpm/package-manager-store/v11");
    std::fs::create_dir_all(&engine).unwrap();
    let policy = admit(&root.join("workspace")).unwrap();
    assert!(
        policy
            .admission_warnings()
            .iter()
            .any(|warning| warning.contains("11.15.1")
                && warning.contains("10.33.1")
                && warning.contains("offline scratch discovery"))
    );
    assert!(
        execute(
            policy,
            &root,
            vec![
                "/usr/bin/touch".into(),
                engine.join("index.db").into_os_string()
            ]
        )
        .is_err()
    );
}

// Catches: no package-manager selector is mistaken for a mismatched pin,
// preventing the installed runtime's legitimate database writes.
#[test]
fn pnpm_11_no_selector_uses_the_installed_runtime_layout() {
    let name = "pnpm_11_layout::pnpm_11_no_selector_uses_the_installed_runtime_layout";
    let Some(root) = isolated(name) else {
        return;
    };
    if !reported_layout_child(&root, name, false, false) {
        return;
    }
    std::fs::write(root.join("workspace/package.json"), "{}").unwrap();
    let engine = root.join("home/Library/pnpm/package-manager-store/v11");
    std::fs::create_dir_all(&engine).unwrap();
    let policy = admit(&root.join("workspace")).unwrap();
    assert!(
        policy.admission_warnings().is_empty(),
        "{:?}",
        policy.admission_warnings()
    );
    execute(
        policy,
        &root,
        vec![
            "/usr/bin/touch".into(),
            engine.join("index.db").into_os_string(),
        ],
    )
    .unwrap();
    assert!(engine.join("index.db").exists());
}

// Catches: discovery silently executes a pinned runtime without confinement,
// or fabricates roots after that runtime requires writes to answer a query.
#[test]
fn pnpm_11_discovery_requiring_writes_warns_without_admitting_roots() {
    let name = "pnpm_11_layout::pnpm_11_discovery_requiring_writes_warns_without_admitting_roots";
    let Some(root) = isolated(name) else {
        return;
    };
    if !reported_layout_child(&root, name, true, false) {
        return;
    }
    let policy = admit(&root.join("workspace")).unwrap();
    assert!(!root.join("workspace/discovery-write").exists());
    assert!(!root.join("home/discovery-write").exists());
    assert!(
        policy
            .admission_warnings()
            .iter()
            .any(|warning| warning.contains("11.15.1")
                && warning.contains("writes outside private scratch and network are disabled")),
        "{:?}",
        policy.admission_warnings()
    );
    assert!(
        !policy
            .adaptive_perimeter()
            .unwrap()
            .writable
            .iter()
            .any(|path| path
                .as_path()
                .to_string_lossy()
                .contains("package-manager-store"))
    );
    let effective =
        ego_core::config::resolve(&[], ego_core::config::Trust::Untrusted, None).unwrap();
    let perimeter = ego_core::health::perimeter::Perimeter::session(&policy, &effective, &[]);
    assert!(perimeter.text().contains("11.15.1"));
}

// Catches: a derived engine sibling symlink becomes a writable unrelated root.
#[test]
fn pnpm_11_symlinked_engine_store_warns_without_opening_the_target() {
    let name = "pnpm_11_layout::pnpm_11_symlinked_engine_store_warns_without_opening_the_target";
    let Some(root) = isolated(name) else {
        return;
    };
    if !reported_layout_child(&root, name, false, false) {
        return;
    }
    let parent = root.join("home/Library/pnpm/package-manager-store");
    std::fs::create_dir_all(&parent).unwrap();
    let target = root.join("home/unrelated");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, parent.join("v11")).unwrap();
    let policy = admit(&root.join("workspace")).unwrap();
    assert!(
        policy
            .admission_warnings()
            .iter()
            .any(|warning| warning.contains("symlinked")),
        "{:?}",
        policy.admission_warnings()
    );
    assert!(
        execute(
            policy,
            &root,
            vec![
                "/usr/bin/touch".into(),
                target.join("index.db").into_os_string()
            ]
        )
        .is_err()
    );
}

// Catches: treating a config-shaped directory as a single readable Git file
// instead opens every private file beneath it in strict workspace confinement.
#[test]
fn workspace_git_ignore_directory_does_not_become_a_runtime_root() {
    let name = "pnpm_11_layout::workspace_git_ignore_directory_does_not_become_a_runtime_root";
    let Some(root) = isolated(name) else {
        return;
    };
    let ignore = root.join("home/.config/git/ignore");
    std::fs::create_dir_all(&ignore).unwrap();
    let secret = ignore.join("private");
    std::fs::write(&secret, "REGRESSION-MARKER").unwrap();
    let policy = admit_confinement(
        Confinement::Workspace,
        false,
        CanonPath::new(root.join("workspace")),
    )
    .unwrap();
    assert!(
        execute(
            policy,
            &root,
            vec!["/bin/cat".into(), secret.into_os_string()]
        )
        .is_err()
    );
}

// Catches: pnpm's otherwise valid layout query needs a cwd-local probe file,
// but discovery runs read-only in the project and therefore admits no roots.
#[test]
fn pnpm_11_discovery_allows_its_private_scratch_probe() {
    let name = "pnpm_11_layout::pnpm_11_discovery_allows_its_private_scratch_probe";
    let Some(root) = isolated(name) else {
        return;
    };
    if !reported_layout_child(&root, name, false, true) {
        return;
    }
    let engine = root.join("home/Library/pnpm/package-manager-store/v11");
    std::fs::create_dir_all(&engine).unwrap();
    let policy = admit(&root.join("workspace")).unwrap();
    assert!(
        policy.admission_warnings().is_empty(),
        "{:?}",
        policy.admission_warnings()
    );
    execute(
        policy,
        &root,
        vec![
            "/usr/bin/touch".into(),
            engine.join("index.db").into_os_string(),
        ],
    )
    .unwrap();
    assert!(!root.join("workspace/discovery-write").exists());
}

// Catches: cached exact-version selection needs its engine lock before the
// reported runtime layout is available, so scratch-only discovery admits none.
#[test]
fn pnpm_11_discovery_bootstraps_only_the_selector_lock() {
    let name = "pnpm_11_layout::pnpm_11_discovery_bootstraps_only_the_selector_lock";
    let Some(root) = isolated(name) else {
        return;
    };
    let engine = root.join("home/Library/pnpm/package-manager-store/v11");
    std::fs::create_dir_all(engine.join("tmp/engine-locks")).unwrap();
    if !reported_layout_child(&root, name, false, false) {
        return;
    }
    let policy = admit(&root.join("workspace")).unwrap();
    assert!(
        policy.admission_warnings().is_empty(),
        "{:?}",
        policy.admission_warnings()
    );
    assert!(
        engine
            .join("tmp/engine-locks/package-manager-env.lock")
            .exists()
    );
    execute(
        policy,
        &root,
        vec![
            "/usr/bin/touch".into(),
            engine.join("index.db").into_os_string(),
        ],
    )
    .unwrap();
}
