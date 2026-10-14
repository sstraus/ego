//! Operator configuration identity captured before native process admission.

use std::path::PathBuf;

use ego_core::permit::ProcessError;

/// Protect the selected spelling as well as its canonical target. In particular,
/// unlinking a symlink must not replace the operator's configuration directory.
pub(super) fn capture() -> Result<Vec<PathBuf>, ProcessError> {
    let selected = std::env::var("EGO_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ego")))
        .ok_or_else(|| {
            ProcessError::Backend("cannot protect user config: EGO_HOME and HOME are absent".into())
        })?;
    let selected = std::path::absolute(selected)
        .map_err(|error| ProcessError::Backend(format!("cannot resolve user config: {error}")))?;
    let canonical = super::adaptive::resolve_filesystem(&selected)?;
    let config_target = super::adaptive::resolve_filesystem(&selected.join("config.toml"))?;
    #[cfg(unix)]
    match std::fs::metadata(&config_target) {
        Ok(metadata) => {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() > 1 {
                return Err(ProcessError::Backend(format!(
                    "user config {} has multiple hardlinks; replace the hardlink with a copy or symlink before sandbox admission",
                    config_target.display()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ProcessError::Backend(format!(
                "cannot inspect user config {}: {error}",
                config_target.display()
            )));
        }
    }
    let mut paths = vec![selected, canonical, config_target];
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Additive Linux backends cannot retain a writable ancestor and subtract the
/// configuration directory. Refuse that policy rather than run it partially.
#[cfg(target_os = "linux")]
pub(super) fn reject_overlap(
    policy: &ego_core::permit::RunConfinement,
    scratch: &std::path::Path,
) -> Result<(), ProcessError> {
    use ego_core::permit::{Confinement, RootAccess};
    if policy.level() != Confinement::Workspace {
        return Ok(());
    }
    let protected = capture()?;
    for writable in [policy.workspace().as_path(), scratch].into_iter().chain(
        policy
            .roots()
            .iter()
            .filter(|root| root.access() == RootAccess::ReadWrite)
            .map(|root| root.path().as_path()),
    ) {
        for directory in &protected {
            if directory.starts_with(writable) || writable.starts_with(directory) {
                return Err(ProcessError::Backend(format!(
                    "user config {} overlaps writable root {}; Linux confinement cannot subtract write authority",
                    directory.display(),
                    writable.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    // Catches: another writable hardlink name bypasses the captured path floor.
    #[test]
    fn user_config_hardlink_refuses_admission() {
        let name = "exec::user_config::tests::user_config_hardlink_refuses_admission";
        if std::env::var_os("EGO_HARDLINK_CASE").is_none() {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env("EGO_HARDLINK_CASE", "1")
                .env("EGO_HOME", root.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        let directory = std::path::PathBuf::from(std::env::var_os("EGO_HOME").unwrap());
        let config = directory.join("config.toml");
        std::fs::write(&config, "mode = 'default'\n").unwrap();
        std::fs::hard_link(&config, directory.join("alias")).unwrap();
        use ego_core::permit::{CanonPath, Confinement, RunConfinement};
        use std::os::unix::fs::PermissionsExt;
        let workspace = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let policy = RunConfinement::new(
            Confinement::Workspace,
            false,
            CanonPath::new(std::fs::canonicalize(workspace.path()).unwrap()),
        )
        .unwrap();
        #[cfg(target_os = "macos")]
        let result = super::super::seatbelt::SeatbeltBackend::prepare(
            &policy,
            &CanonPath::new(scratch.path()),
            &super::super::seatbelt::derived_runtime_roots().unwrap(),
        );
        #[cfg(target_os = "linux")]
        let result = super::super::landlock::LandlockBackend::prepare(
            &policy,
            &CanonPath::new(scratch.path()),
            &[],
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("multiply-linked config admitted"),
        };
        assert!(error.to_string().contains("hardlink"), "{error}");
        assert_eq!(std::fs::read(config).unwrap(), b"mode = 'default'\n");
    }
}
