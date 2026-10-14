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
    let canonical = super::adaptive::resolve(&selected)?;
    let mut paths = vec![selected, canonical];
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
