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
    std::fs::write(&ignore, "GIT-IGNORE-ALI").unwrap();
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
                .any(|block| block.text.contains("GIT-IGNORE-ALI"))
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
        r#"{"name":"sandbox-ALI-live","private":true,"packageManager":"pnpm@11.15.1"}"#,
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
    std::fs::write(&secret, "DIRECTORY-NEIGHBOR-ALI").unwrap();
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

// Catches: Adaptive changes pnpm's dead-owner lock behavior compared with an
// unsandboxed run. Clone the cached engine so no operator lock is touched.
#[test]
#[ignore = "live installed pnpm@11.15.1 and APFS clone prerequisite; run explicitly"]
fn pnpm_11_live_dead_selector_lock_matches_unsandboxed_behavior() {
    const NAME: &str =
        "pnpm_11_layout::pnpm_11_live_dead_selector_lock_matches_unsandboxed_behavior";
    if let Some(root) = std::env::var_os("EGO_PNPM_STALE_ROOT") {
        let root = PathBuf::from(root);
        let workspace = root.join("workspace");
        let policy = admit_confinement(
            Confinement::Adaptive,
            false,
            CanonPath::new(workspace.canonicalize().unwrap()),
        )
        .unwrap();
        let witness = |level| {
            let policy = if level == Confinement::Off {
                ego_core::permit::RunConfinement::new(level, false, policy.workspace().clone())
                    .unwrap()
            } else {
                policy.clone()
            };
            execute(
                policy,
                &root,
                vec![
                    "/opt/homebrew/bin/pnpm".into(),
                    "exec".into(),
                    "/usr/bin/true".into(),
                ],
            )
        };
        // Establish that the real copied cached engine works before poisoning
        // only its task-owned lock; this is not an engine-install test.
        witness(Confinement::Adaptive).expect("isolated cached pnpm baseline within 10 seconds");
        let mut owner = Command::new("/usr/bin/true").spawn().unwrap();
        let pid = owner.id();
        assert!(owner.wait().unwrap().success());
        assert_eq!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
            Err(nix::errno::Errno::ESRCH),
            "fixture owner must be dead"
        );
        let lock =
            root.join("pnpm/package-manager-store/v11/tmp/engine-locks/package-manager-env.lock");
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::create_dir(&lock).expect("baseline must release its selector lock");
        // Match the real owner format recorded in the stale-selector fixture.
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let value = format!("{pid}-{timestamp}-0");
        std::fs::write(lock.join("owner"), &value).unwrap();
        // Recorded unsandboxed fixture: pnpm also retains a fresh abandoned
        // owner beyond 10 seconds. Compare both policies on the same lock.
        for level in [Confinement::Off, Confinement::Adaptive] {
            let started = std::time::Instant::now();
            let output = witness(level);
            assert!(
                matches!(
                    output,
                    Err(ego_core::permit::ProcessError::Timeout { timeout })
                        if timeout == std::time::Duration::from_secs(10)
                ),
                "{level} must match the recorded pnpm 10s timeout: {output:?}; elapsed={:?}",
                started.elapsed()
            );
            assert_eq!(
                std::fs::read_to_string(lock.join("owner")).unwrap(),
                value,
                "{level} must leave the same dead owner untouched"
            );
        }
        return;
    }
    let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("package.json"),
        r#"{"name":"sandbox-ALI-stale-lock","private":true,"packageManager":"pnpm@11.15.1"}"#,
    )
    .unwrap();
    let home = std::env::var_os("PNPM_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(|root| PathBuf::from(root).join("pnpm")))
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join("Library/pnpm"));
    let pnpm = root.path().join("pnpm");
    std::fs::create_dir(&pnpm).unwrap();
    let copy = Command::new("/bin/cp")
        .arg("-cR")
        .arg(home.join("package-manager-store"))
        .arg(&pnpm)
        .output()
        .unwrap();
    assert!(copy.status.success(), "APFS cached-engine clone: {copy:?}");
    let tmp = pnpm.join("package-manager-store/v11/tmp");
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp).unwrap();
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("adaptive_admission::{NAME}"),
            "--ignored",
            "--nocapture",
        ])
        .env("EGO_PNPM_STALE_ROOT", root.path())
        .env("PNPM_HOME", &pnpm)
        .output()
        .unwrap();
    reexec::assert_one_child_test(&output);
}
