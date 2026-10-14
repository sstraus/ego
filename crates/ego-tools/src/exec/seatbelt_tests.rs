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

// Catches: a read root widens native writes/exec, follows escaped links, or lifts a floor.
#[test]
fn upload_read_root_preserves_native_write_exec_and_credential_floors() {
    use ego_core::permit::{ProfileRoot, RootAccess};
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let workspace = base.join("workspace");
    let scratch = base.join("scratch");
    let uploads = base.join("uploads");
    let credentials = uploads.join(".ssh");
    for path in [&workspace, &scratch, &uploads, &credentials] {
        fs::create_dir(path).unwrap();
    }
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
    let attachment = uploads.join("attachment");
    fs::write(&attachment, "NATIVE-UPLOAD").unwrap();
    fs::write(credentials.join("synthetic"), "FLOOR-SECRET").unwrap();
    let outside = base.join("outside");
    fs::write(&outside, "OUTSIDE-SECRET").unwrap();
    std::os::unix::fs::symlink(&outside, uploads.join("escape")).unwrap();
    let executable = uploads.join("program");
    fs::copy("/bin/echo", &executable).unwrap();
    for level in [
        Confinement::Workspace,
        Confinement::Ro,
        Confinement::Adaptive,
    ] {
        let policy = RunConfinement::new(level, false, CanonPath::new(&workspace))
            .unwrap()
            .with_roots(vec![ProfileRoot::new(
                CanonPath::new(&uploads),
                RootAccess::Read,
            )])
            .with_adaptive_perimeter(AdaptivePerimeter {
                home: CanonPath::new(&base),
                writable: vec![CanonPath::new(&workspace)],
                credentials: vec![CanonPath::new(&credentials)],
                controls: vec![],
            });
        let backend = SeatbeltBackend::prepare(
            &policy,
            &CanonPath::new(&scratch),
            &derived_runtime_roots().unwrap(),
        )
        .unwrap();
        let read = run(&backend, "/bin/cat", &[attachment.as_os_str()]);
        assert!(read.status.success(), "{level}: {read:?}");
        assert_eq!(read.stdout, b"NATIVE-UPLOAD");
        let write = run(
            &backend,
            "/bin/sh",
            &[
                "-c".as_ref(),
                "printf changed > \"$1\"".as_ref(),
                "write".as_ref(),
                attachment.as_os_str(),
            ],
        );
        assert!(!write.status.success(), "{level}: {write:?}");
        assert_eq!(fs::read(&attachment).unwrap(), b"NATIVE-UPLOAD");
        let exec = run(&backend, executable.to_str().unwrap(), &["RAN".as_ref()]);
        assert!(!exec.status.success(), "{level}: {exec:?}");
        if level != Confinement::Adaptive {
            assert!(
                !run(&backend, "/bin/cat", &[uploads.join("escape").as_os_str()])
                    .status
                    .success()
            );
        }
        if level == Confinement::Adaptive {
            assert!(
                !run(
                    &backend,
                    "/bin/cat",
                    &[credentials.join("synthetic").as_os_str()]
                )
                .status
                .success()
            );
        }
    }
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
        .widened(
            &[SandboxGrant {
                path: CanonPath::new(granted),
                access: SandboxAccess::Write,
            }],
            &[],
        )
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
