use ego_core::permit::{CanonPath, EnvName, EnvPolicy, ExplicitEnvBinding, SpawnSpec};

fn captured(script: &str, path: &str, cwd: &std::path::Path) -> SpawnSpec {
    let environment = EnvPolicy::exact(vec![
        ExplicitEnvBinding::from_native(EnvName::new("PATH").unwrap(), path.into()).unwrap(),
    ])
    .unwrap();
    super::capture(
        &SpawnSpec::new(
            vec!["/bin/sh".into(), "-c".into(), script.into()],
            Some(CanonPath::new(cwd)),
            None,
            environment,
        )
        .unwrap(),
    )
    .unwrap()
}

// Catches: host capture observes only the outer shell, disabling real trusted wrapper approval.
#[test]
fn capture_observes_inner_system_ls_before_read_only_approval() {
    let directory = tempfile::tempdir().unwrap();
    let spec = captured("ls README.md", "/usr/bin:/bin", directory.path());
    assert!(spec.identity().unwrap().files.len() >= 2);
    assert!(ego_core::permit::shell::trusted_read_only(&spec));
    super::revalidate(&spec).unwrap();
}

// Catches: actual captured PATH resolution lends authority to a workspace ls executable.
#[test]
fn capture_of_workspace_path_shadow_keeps_shell_untrusted() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("ls");
    std::fs::write(&executable, "#!/bin/sh\nprintf changed > changed.txt\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", directory.path().display());
    let spec = captured("ls README.md", &path, directory.path());
    assert!(
        spec.identity()
            .unwrap()
            .files
            .iter()
            .any(|file| file.named.as_path() == executable)
    );
    assert!(!ego_core::permit::shell::trusted_read_only(&spec));
    assert!(!directory.path().join("changed.txt").exists());
}
