use super::*;

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

// Catches: static grants still protect the host-engine links or omit a global
// pnpm environment, even though the approved contract opens the whole home.
#[test]
fn pnpm_home_is_writable_for_workspace_and_adaptive_without_discovery() {
    let name = "pnpm_11_layout::pnpm_home_is_writable_for_workspace_and_adaptive_without_discovery";
    let Some(root) = isolated(name) else {
        return;
    };
    let pnpm = root.join("home/Library/pnpm");
    for level in [Confinement::Workspace, Confinement::Adaptive] {
        let policy =
            admit_confinement(level, false, CanonPath::new(root.join("workspace"))).unwrap();
        for suffix in [
            "package-manager-store/v11/index.db",
            "package-manager-store/v11/links/engine",
            "global/v11/probe",
            "bin/probe",
        ] {
            let path = pnpm.join(suffix);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            execute(
                policy.clone(),
                &root,
                vec!["/usr/bin/touch".into(), path.clone().into_os_string()],
            )
            .unwrap();
            assert!(path.exists());
        }
    }
}
