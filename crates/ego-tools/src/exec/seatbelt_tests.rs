use super::*;
use ego_core::permit::{AdaptivePerimeter, SandboxAccess};
use std::process::{Command, Output};

fn run(backend: &SeatbeltBackend, program: &str, args: &[&std::ffi::OsStr]) -> Output {
    let prefix = backend.launch_prefix();
    Command::new(prefix.program.unwrap())
        .args(prefix.args_before_target)
        .arg(program)
        .args(args)
        .output()
        .unwrap()
}

// Catches: a writable on-disk policy lets one child unconfine a later spawn,
// including a widened backend, by overwriting or replacing the policy file.
#[test]
fn adaptive_policy_tampering_cannot_unconfine_later_spawns() {
    let root = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(root.path()).unwrap();
    let workspace = base.join("workspace");
    let scratch = base.join("scratch");
    let credentials = base.join("credentials");
    for path in [&workspace, &scratch, &credentials] {
        fs::create_dir(path).unwrap();
    }
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
    let secret = credentials.join("synthetic");
    fs::write(&secret, "synthetic credential").unwrap();
    let policy = RunConfinement::new(
        Confinement::Adaptive,
        false,
        CanonPath::new(workspace.clone()),
    )
    .unwrap()
    .with_adaptive_perimeter(AdaptivePerimeter {
        home: CanonPath::new(base.clone()),
        writable: vec![CanonPath::new(
            fs::canonicalize(std::env::temp_dir()).unwrap(),
        )],
        credentials: vec![CanonPath::new(credentials)],
        controls: vec![],
    });
    let backend = SeatbeltBackend::prepare(&policy, &CanonPath::new(scratch), &[]).unwrap();
    let granted = base.join("granted");
    fs::create_dir(&granted).unwrap();
    let widened = backend
        .widened(&[SandboxGrant {
            path: CanonPath::new(granted),
            access: SandboxAccess::Write,
        }])
        .unwrap();
    // TMPDIR is intentionally writable, as in production Adaptive. Use a
    // synthetic credential floor to observe escape without reading real secrets.
    for backend in [&backend, &widened] {
        let prefix = backend.launch_prefix();
        let target = if prefix.args_before_target[0] == "-f" {
            PathBuf::from(&prefix.args_before_target[1])
        } else {
            workspace.join("policy-decoy.sb")
        };
        for attack in [
            "printf '(version 1)(allow default)' > \"$1\"",
            "rm -f \"$1\"; printf '(version 1)(allow default)' > \"$1\"",
        ] {
            let output = run(
                backend,
                "/bin/sh",
                &[
                    "-c".as_ref(),
                    attack.as_ref(),
                    "attack".as_ref(),
                    target.as_os_str(),
                ],
            );
            assert!(output.status.success(), "{output:?}");
            let output = run(backend, "/bin/cat", &[secret.as_os_str()]);
            assert!(
                !output.status.success(),
                "policy tampering exposed credentials"
            );
            let forbidden = secret.with_file_name("forbidden-write");
            let output = run(backend, "/usr/bin/touch", &[forbidden.as_os_str()]);
            assert!(
                !output.status.success(),
                "policy tampering allowed a forbidden write"
            );
            assert!(!forbidden.exists());
        }
    }
}
