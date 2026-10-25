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
    ) {
        return;
    }
    // Layout recorded from pnpm@11.15.1 in the story's live scratch fixture.
    let store = root.join("home/Library/pnpm/package-manager-store/v11");
    let links = store.join("links/@/pnpm/11.15.1/bin");
    std::fs::create_dir_all(&links).unwrap();
    let engine = links.join("pnpm");
    std::fs::write(&engine, "host engine").unwrap();
    let policy = admit(&root.join("workspace")).unwrap();
    for path in [
        store.join("index.db"),
        store.join("pnpm-store-operation-locks"),
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
        "real pnpm@11.15.1 must execute through the admitted perimeter: {output:?}"
    );
}

// This is a recorded-output adapter at the discovery boundary, not the live
// pnpm test below. Every policy assertion still executes real Seatbelt children.
fn reported_layout_child(root: &Path, name: &str, require_write: bool) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    if std::env::var_os("EGO_PNPM_REPORTED_LAYOUT").is_some() {
        return true;
    }
    let bin = root.join("operator-bin");
    std::fs::create_dir(&bin).unwrap();
    let script = if require_write {
        "#!/bin/sh\nexec /usr/bin/touch \"$PWD/discovery-write\"\n".to_owned()
    } else {
        r#"#!/bin/sh
case "$1 $2" in
  'store path') printf '%s\n' "$HOME/Library/pnpm/store/v11" ;;
  'root -g') printf '%s\n' "$HOME/Library/pnpm/global/v11" ;;
  'bin -g') printf '%s\n' "$HOME/Library/pnpm/bin" ;;
  *) exit 1 ;;
esac
"#
        .to_owned()
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

// Catches: discovery silently executes a pinned runtime without confinement,
// or fabricates roots after that runtime requires writes to answer a query.
#[test]
fn pnpm_11_discovery_requiring_writes_warns_without_admitting_roots() {
    let name = "pnpm_11_layout::pnpm_11_discovery_requiring_writes_warns_without_admitting_roots";
    let Some(root) = isolated(name) else {
        return;
    };
    if !reported_layout_child(&root, name, true) {
        return;
    }
    let policy = admit(&root.join("workspace")).unwrap();
    assert!(!root.join("workspace/discovery-write").exists());
    assert!(
        policy
            .admission_warnings()
            .iter()
            .any(|warning| warning.contains("11.15.1")
                && warning.contains("runtime writes and network are disabled")),
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
    if !reported_layout_child(&root, name, false) {
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
