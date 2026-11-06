//! Operator configuration identity captured before native process admission.

use std::path::{Path, PathBuf};

use ego_core::permit::ProcessError;

/// Upper bound on directory entries visited while looking for hardlink aliases.
const ALIAS_SCAN_LIMIT: usize = 1_000_000;

/// Protect the selected spelling as well as its canonical target. In particular,
/// unlinking a symlink must not replace the operator's configuration directory.
pub(super) fn capture() -> Result<Vec<PathBuf>, ProcessError> {
    capture_with_file().map(|(paths, _)| paths)
}

/// The protected paths and the resolved configuration file among them.
fn capture_with_file() -> Result<(Vec<PathBuf>, PathBuf), ProcessError> {
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
    let mut paths = vec![selected, canonical, config_target.clone()];
    paths.sort();
    paths.dedup();
    Ok((paths, config_target))
}

/// The protected paths plus every other name of a multiply linked configuration
/// file inside a writable root. A hardlink alias shares the inode, so a path deny
/// on the configuration alone would let the alias rewrite it. A name created after
/// this capture is outside it; the backend's own link and rename rules cover that.
pub(super) fn capture_for(writable: &[&Path]) -> Result<Vec<PathBuf>, ProcessError> {
    let (mut paths, config) = capture_with_file()?;
    paths.extend(aliases(&config, writable)?);
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Writable roots of a policy, in the order backends grant them.
pub(super) fn writable_roots<'a>(
    policy: &'a ego_core::permit::RunConfinement,
    scratch: &'a Path,
) -> Vec<&'a Path> {
    use ego_core::permit::RootAccess;
    [policy.workspace().as_path(), scratch]
        .into_iter()
        .chain(
            policy
                .roots()
                .iter()
                .filter(|root| root.access() == RootAccess::ReadWrite)
                .map(|root| root.path().as_path()),
        )
        .chain(
            policy
                .adaptive_perimeter()
                .into_iter()
                .flat_map(|perimeter| perimeter.writable.iter().map(|path| path.as_path())),
        )
        .collect()
}

#[cfg(unix)]
fn aliases(config: &Path, writable: &[&Path]) -> Result<Vec<PathBuf>, ProcessError> {
    use std::os::unix::fs::MetadataExt;
    let identity = match std::fs::metadata(config) {
        Ok(metadata) if metadata.nlink() > 1 => (metadata.dev(), metadata.ino()),
        Ok(_) => return Ok(Vec::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(ProcessError::Backend(format!(
                "cannot inspect user config {}: {error}",
                config.display()
            )));
        }
    };
    let mut found = Vec::new();
    let mut visited = 0_usize;
    let mut pending = writable
        .iter()
        .map(|root| root.to_path_buf())
        .collect::<Vec<_>>();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > ALIAS_SCAN_LIMIT {
                return Err(ProcessError::Backend(format!(
                    "user config {} has multiple hardlinks and the writable roots hold more than {ALIAS_SCAN_LIMIT} entries; cannot prove no alias is writable",
                    config.display()
                )));
            }
            let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if (metadata.dev(), metadata.ino()) == identity && entry.path() != config {
                found.push(entry.path());
            }
        }
    }
    Ok(found)
}

#[cfg(not(unix))]
fn aliases(_config: &Path, _writable: &[&Path]) -> Result<Vec<PathBuf>, ProcessError> {
    Ok(Vec::new())
}

/// bwrap mounts cannot subtract an absent path or an alias, so it refuses an
/// overlapping policy and admission falls through to Landlock, which enumerates
/// around the protected paths.
#[cfg(target_os = "linux")]
pub(super) fn reject_overlap(
    policy: &ego_core::permit::RunConfinement,
    scratch: &Path,
) -> Result<(), ProcessError> {
    use ego_core::permit::Confinement;
    if policy.level() != Confinement::Workspace {
        return Ok(());
    }
    let writable = writable_roots(policy, scratch);
    let protected = capture_for(&writable)?;
    for writable in writable {
        for directory in &protected {
            if directory.starts_with(writable) || writable.starts_with(directory) {
                return Err(ProcessError::Backend(format!(
                    "user config {} overlaps writable root {}; bwrap cannot subtract it",
                    directory.display(),
                    writable.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::aliases;

    // Catches: another writable hardlink name bypasses the captured path floor,
    // or the scan reports unrelated files, symlinks or the configuration itself.
    #[test]
    fn writable_hardlink_names_of_the_config_are_found_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("ego/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "mode = 'default'\n").unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(workspace.join("deep/er")).unwrap();
        std::fs::write(workspace.join("unrelated"), "x").unwrap();
        std::os::unix::fs::symlink(&config, workspace.join("symlink")).unwrap();
        assert!(aliases(&config, &[workspace.as_path()]).unwrap().is_empty());
        let alias = workspace.join("deep/er/alias");
        std::fs::hard_link(&config, &alias).unwrap();
        assert_eq!(aliases(&config, &[workspace.as_path()]).unwrap(), [alias]);
        // The configuration's own directory is not an alias of itself.
        let own = [config.parent().unwrap()];
        assert!(aliases(&config, &own).unwrap().is_empty());
    }
}
