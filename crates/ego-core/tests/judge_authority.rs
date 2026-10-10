use ego_core::permit::analysis::{ExecutableFileIdentity, ExecutionIdentity};
use ego_core::permit::shell::trusted_read_only;
use ego_core::permit::{CanonPath, EnvPolicy, SpawnSpec};

fn captured(argv: &[&str], programs: &[&str]) -> SpawnSpec {
    let target = CanonPath::new(argv[0]);
    SpawnSpec::new(
        argv.iter().map(|word| (*word).into()).collect(),
        Some(CanonPath::new("/ws")),
        None,
        EnvPolicy::Inherited,
    )
    .unwrap()
    .with_execution_identity(
        EnvPolicy::exact(vec![]).unwrap(),
        ExecutionIdentity {
            target: target.clone(),
            files: std::iter::once(argv[0])
                .chain(programs.iter().copied())
                .map(|path| ExecutableFileIdentity {
                    named: CanonPath::new(path),
                    resolved: CanonPath::new(path),
                    content: Some([1; 32]),
                    platform: vec![1],
                })
                .collect(),
            failures: vec![],
            dynamic_code: argv[0] == "/bin/bash",
        },
    )
}

// Catches: hidden sensitive filenames and implicit recursive selection gain read authority.
#[test]
fn sensitive_shell_selection_cannot_gain_read_only_authority() {
    for script in [
        "/bin/cat .e\"\"nv",
        "/bin/cat .en?",
        "/bin/cat .e*",
        "/usr/bin/grep -r secret .",
        "/usr/bin/rg secret .",
        "/usr/bin/rg secret",
    ] {
        assert!(
            !trusted_read_only(&captured(
                &["/bin/bash", "-c", script],
                &["/bin/cat", "/usr/bin/grep", "/usr/bin/rg"],
            )),
            "unsafe selection received authority: {script}"
        );
    }
}

// Catches: escaped option spelling is treated as data although bash removes the escape.
#[test]
fn escaped_shell_options_cannot_gain_read_only_authority() {
    for script in [
        r"/usr/bin/find . \-delete",
        r"/usr/bin/find . \-exec /bin/echo changed ;",
        r"/usr/bin/rg \--pre /bin/sh .",
        r"/usr/bin/sed -n 1p README.md \-i",
    ] {
        assert!(
            !trusted_read_only(&captured(
                &["/bin/bash", "-c", script],
                &["/usr/bin/find", "/usr/bin/rg", "/usr/bin/sed"],
            )),
            "escaped option received authority: {script}"
        );
    }
    // Native argv retains a literal backslash, unlike shell source spelling.
    assert!(trusted_read_only(&captured(
        &["/usr/bin/find", ".", r"\-delete"],
        &[],
    )));
}

// Catches: git status can execute core.fsmonitor while bypassing the judge as read-only.
#[test]
fn git_status_cannot_gain_read_only_authority() {
    for argv in [
        vec!["/usr/bin/git", "status"],
        vec!["/usr/bin/git", "status", "--short"],
        vec!["/bin/bash", "-c", "/usr/bin/git status --porcelain"],
    ] {
        assert!(
            !trusted_read_only(&captured(&argv, &["/usr/bin/git"])),
            "git status received authority: {argv:?}"
        );
    }
}
