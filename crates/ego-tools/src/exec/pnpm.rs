//! Static operator pnpm authority; config paths are only human-consent hints.
use ego_core::permit::ProcessError;
use std::io::Read as _;
use std::path::{Path, PathBuf};

pub(super) fn home(home: &Path) -> PathBuf {
    let set = |key| {
        std::env::var_os(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    set("PNPM_HOME")
        .or_else(|| set("XDG_DATA_HOME").map(|root| root.join("pnpm")))
        .unwrap_or_else(|| home.join("Library/pnpm"))
}

pub(super) fn roots(home: &Path, workspace: &Path) -> Result<[PathBuf; 2], ProcessError> {
    let pnpm = super::adaptive::resolve(&self::home(home))?;
    let uid = nix::unistd::getuid().as_raw();
    let locks = PathBuf::from(format!("/private/tmp/pnpm-store-operation-locks-{uid}"));
    validate_operation_locks(&locks, uid)?;
    for path in [&pnpm, &locks] {
        super::adaptive::check_write_root(home, workspace, path, &[])?;
    }
    Ok([pnpm, locks])
}

fn validate_operation_locks(path: &Path, uid: u32) -> Result<(), ego_core::permit::ProcessError> {
    use std::os::unix::fs::MetadataExt as _;
    if super::adaptive::resolve(path)? != path {
        return Err(super::adaptive::error(
            "pnpm operation-lock directory contains a symlink",
        ));
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0 =>
        {
            Ok(())
        }
        Ok(_) => Err(super::adaptive::error(
            "pnpm operation-lock directory is not private and user-owned",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(super::adaptive::error(format!(
            "cannot inspect pnpm operation locks: {error}"
        ))),
    }
}

// Bounded reads only: no pnpm, shell, environment interpolation or project code.
fn text(path: &Path) -> Option<String> {
    let mut value = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(65537)
        .read_to_string(&mut value)
        .ok()?;
    (value.len() <= 65536).then_some(value)
}

pub(super) fn custom_store(home: &Path, workspace: &Path) -> Option<PathBuf> {
    let npmrc = |path: &Path| {
        text(path)?
            .lines()
            .filter_map(|line| {
                let (key, value) = line.trim().split_once('=')?;
                (key.trim() == "store-dir")
                    .then(|| value.trim().trim_matches(['\"', '\'']).to_owned())
            })
            .last()
    };
    let yaml = text(&workspace.join("pnpm-workspace.yaml"))
        .and_then(|text| yaml_rust2::YamlLoader::load_from_str(&text).ok())
        .and_then(|docs| docs.first()?["storeDir"].as_str().map(str::to_owned));
    let value = yaml
        .or_else(|| npmrc(&workspace.join(".npmrc")))
        .or_else(|| npmrc(&home.join(".npmrc")))?;
    if value.is_empty() || value.contains("${") {
        return None;
    }
    let path = value
        .strip_prefix("~/")
        .map_or_else(|| workspace.join(&value), |suffix| home.join(suffix));
    let path = super::adaptive::resolve_filesystem(&path).ok()?;
    let pnpm = super::adaptive::resolve(&self::home(home)).ok()?;
    (!path.starts_with(pnpm) && !path.starts_with(workspace)).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Catches: a pre-created operation-lock alias or foreign/shared directory
    // launders authority over unrelated temporary files.
    #[test]
    fn operation_lock_directory_refuses_aliases_and_unsafe_ownership() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("locks");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let uid = nix::unistd::getuid().as_raw();
        assert!(validate_operation_locks(&directory, uid).is_ok());
        assert!(validate_operation_locks(&directory, uid + 1).is_err());
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(validate_operation_locks(&directory, uid).is_err());
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = root.path().join("alias");
        symlink(&directory, &alias).unwrap();
        assert!(validate_operation_locks(&alias, uid).is_err());
        let file = root.path().join("file");
        std::fs::write(&file, "not a directory").unwrap();
        assert!(validate_operation_locks(&file, uid).is_err());
        assert!(validate_operation_locks(&root.path().join("absent"), uid).is_ok());
    }

    // Catches: config is executed or a configured external store is silently
    // granted, rather than only returned as data for the existing consent path.
    #[test]
    fn custom_store_config_is_data_with_workspace_precedence() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(home.join(".npmrc"), "store-dir=~/external-user\n").unwrap();
        assert_eq!(
            custom_store(&home, &workspace),
            Some(home.join("external-user"))
        );
        std::fs::write(workspace.join(".npmrc"), "store-dir=../project-store\n").unwrap();
        assert_eq!(
            custom_store(&home, &workspace),
            Some(root.path().join("project-store"))
        );
        std::fs::write(
            workspace.join("pnpm-workspace.yaml"),
            "storeDir: '../yaml-store'\n",
        )
        .unwrap();
        assert_eq!(
            custom_store(&home, &workspace),
            Some(root.path().join("yaml-store"))
        );
        std::fs::write(
            workspace.join("pnpm-workspace.yaml"),
            "storeDir: './inside'\n",
        )
        .unwrap();
        assert_eq!(custom_store(&home, &workspace), None);
        assert!(!root.path().join("yaml-store").exists());
    }
}
