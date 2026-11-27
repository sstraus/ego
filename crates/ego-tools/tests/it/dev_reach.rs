//! Native regressions: ordinary development reach under Adaptive and Workspace.
//! Each case runs in a child process whose HOME is a synthetic tree.

#![cfg(target_os = "macos")]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use ego_core::event::id::SessionId;
use ego_core::permit::{CanonPath, Confinement, EnvPolicy, SpawnSpec};
use ego_core::turn::CancelToken;
use ego_tools::exec::{
    ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts, SessionEvidenceStore,
    admit_confinement,
};
use ego_tools::http::SystemResolver;

fn isolated(name: &str, level: &str) {
    if std::env::var("EGO_DEV_CASE").as_deref() != Ok(name) {
        // Short: a unix socket path holds about 100 bytes.
        let sock =
            std::env::temp_dir().join(format!("ego-dev-{}-{level}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let root = tempfile::Builder::new()
            .prefix(".ego-dev-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        for dir in ["home", "workspace", "tmp", "cargo", "rustup", "ego"] {
            std::fs::create_dir(root.path().join(dir)).unwrap();
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("dev_reach::{name}"), "--nocapture"])
            .env("EGO_DEV_CASE", name)
            .env("EGO_DEV_LEVEL", level)
            .env("EGO_DEV_ROOT", root.path())
            .env("HOME", root.path().join("home"))
            .env("TMPDIR", root.path().join("tmp"))
            .env("CARGO_HOME", root.path().join("cargo"))
            .env("RUSTUP_HOME", root.path().join("rustup"))
            .env("EGO_HOME", root.path().join("ego"))
            .env("TUIC_SESSION", "dev-reach")
            .env("EGO_DEV_OUTSIDE_TMP", root.path().join("tmp"))
            .env("SSH_AUTH_SOCK", &sock)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    root.path().join("home/bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated case failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = std::env::var_os("EGO_DEV_ROOT").unwrap();
    if name.starts_with("workspace_git") {
        run_git_case(Path::new(&root));
    } else {
        run_case(Path::new(&root), level);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// Catches: git status and commit fail in a linked worktree under the workspace
// sandbox because the common git directory is outside every root.
fn run_git_case(root: &Path) {
    use ego_core::permit::{ProfileRoot, RootAccess};
    let base = std::fs::canonicalize(root).unwrap();
    let main = base.join("main");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "first"]);
    git(&main, &["worktree", "add", "-q", "../wt", "-b", "wt"]);
    let wt = base.join("wt");
    let workspace = CanonPath::new(&wt);
    let roots: Vec<_> = ego_tools::exec::same_repository_roots(&wt)
        .into_iter()
        .filter(|root| !root.is_or_under(&workspace))
        .map(|root| ProfileRoot::new(root, RootAccess::ReadWrite))
        .collect();
    let policy = admit_confinement(Confinement::Workspace, false, workspace)
        .unwrap()
        .with_roots(roots);
    let evidence =
        SessionEvidenceStore::open(base.join("evidence"), SessionId::new(), 1 << 20).unwrap();
    let config = ExecRunConfig::new(
        policy,
        evidence,
        CancelToken::new(),
        ExecLimits::default(),
        HttpCallTimeout::new(Duration::from_secs(1)).unwrap(),
        None,
        Arc::new(SystemResolver::new()),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (ports, _, _) = RunEffectPorts::for_run(config).await.unwrap();
        let run = |script: &str| {
            SpawnSpec::new(
                vec!["/bin/sh".into(), "-c".into(), script.into()],
                Some(CanonPath::new(&wt)),
                Some(Duration::from_secs(90)),
                EnvPolicy::Inherited,
            )
            .unwrap()
        };
        let status = ports.process().execute(&run("/usr/bin/git status --short --branch")).await;
        assert!(status.is_ok(), "git status: {status:?}");
        let commit = ports
            .process()
            .execute(&run(
                "echo x >> f && /usr/bin/git add f && /usr/bin/git -c user.name=t -c user.email=t@t -c maintenance.auto=false -c gc.auto=0 commit -q --no-verify -m second",
            ))
            .await;
        assert!(commit.is_ok(), "git commit: {commit:?}");
    });
    let log = Command::new("git")
        .current_dir(&wt)
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "second");
}

fn run_case(root: &Path, level: &str) {
    let home = root.join("home");
    let write = |path: &Path, text: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(&home.join(".ssh/id_ed25519.pub"), "public");
    write(&home.join(".ssh/allowed_signers"), "signers");
    write(&home.join(".ssh/id_ed25519"), "private");
    write(&home.join("other/file"), "other");
    let mbx = home.join("Library/Application Support/mbx");
    let cache = home.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    write(
        &mbx.join("config.toml"),
        &format!("cache_dir = \"{}\"\n", cache.display()),
    );
    let shim = mbx.join("bin/cargo");
    write(&shim, "#!/bin/sh\necho built > \"$1\"\n");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let tuic = home.join("bin/tuic");
    write(&tuic, "#!/bin/sh\necho tuic-ok\n");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tuic, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // The agent of ssh signing: a unix socket outside every root.
    let sock = std::path::PathBuf::from(std::env::var_os("SSH_AUTH_SOCK").unwrap());
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let served = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let (mut stream, _) = listener.accept().unwrap();
        let mut byte = [0_u8; 4];
        stream.read_exact(&mut byte).unwrap();
        stream.write_all(b"pong").unwrap();
    });
    let level = if level == "workspace" {
        Confinement::Workspace
    } else {
        Confinement::Adaptive
    };
    let policy = admit_confinement(
        level,
        false,
        CanonPath::new(std::fs::canonicalize(root.join("workspace")).unwrap()),
    )
    .unwrap();
    let evidence =
        SessionEvidenceStore::open(root.join("evidence"), SessionId::new(), 1024 * 1024).unwrap();
    let config = ExecRunConfig::new(
        policy,
        evidence,
        CancelToken::new(),
        ExecLimits::default(),
        HttpCallTimeout::new(Duration::from_secs(1)).unwrap(),
        None,
        Arc::new(SystemResolver::new()),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (ports, _, _) = RunEffectPorts::for_run(config).await.unwrap();
        let sh = |script: String| {
            SpawnSpec::new(
                vec!["/bin/sh".into(), "-c".into(), script.into()],
                None,
                Some(Duration::from_secs(5)),
                EnvPolicy::Inherited,
            )
            .unwrap()
        };
        let ok = |script: String| {
            let ports = &ports;
            async move { ports.process().execute(&sh(script)).await.is_ok() }
        };
        // Catches: SSH signing cannot read the public key or the signers file.
        assert!(
            ok(format!(
                "cat '{}'",
                home.join(".ssh/id_ed25519.pub").display()
            ))
            .await
        );
        assert!(
            ok(format!(
                "cat '{}'",
                home.join(".ssh/allowed_signers").display()
            ))
            .await
        );
        // Catches: the private key opened beside its public half.
        assert!(!ok(format!("cat '{}'", home.join(".ssh/id_ed25519").display())).await);
        // Catches: the cargo shim of mbx cannot run, or its cache is not writable.
        let target = cache.join("targets-probe");
        assert!(
            ok(format!("'{}' '{}'", shim.display(), target.display())).await,
            "mbx shim must run and write its cache"
        );
        assert!(target.is_file());
        // Catches: the tuic CLI of a TUICommander session resolves outside the exec perimeter.
        assert!(ok("tuic".to_owned()).await, "tuic must run");
        // Catches: a sandbox refusal reported as the tool's own error only. The
        // result must say the sandbox blocked the path, and name it.
        let refused = ports
            .process()
            .execute(&sh(format!(
                "cat '{}'",
                home.join(".ssh/id_ed25519").display()
            )))
            .await;
        let text = match refused {
            Err(ego_core::permit::ProcessError::Exit { result, .. }) => result
                .content
                .iter()
                .map(|block| block.text.clone())
                .collect::<String>(),
            other => panic!("the private key read must fail: {other:?}"),
        };
        assert!(
            text.contains("id_ed25519") && text.contains("sandbox"),
            "refusal must name the path and the sandbox: {text}"
        );
        assert!(!text.contains("No such file"), "{text}");
        // Catches: SSH_AUTH_SOCK unreachable, so signing and git over ssh fail.
        let agent = ports
            .process()
            .execute(&sh(
                "printf ping | /usr/bin/nc -U \"$SSH_AUTH_SOCK\"".to_owned()
            ))
            .await;
        let agent_text = match agent {
            Ok(result) => result
                .content
                .iter()
                .map(|b| b.text.clone())
                .collect::<String>(),
            Err(error) => panic!("agent socket: {error:?}"),
        };
        assert!(agent_text.contains("pong"), "{agent_text}");
        // Catches: a tool's own nested sandbox failing with a bare EPERM that
        // names no path and no remedy (SwiftPM manifest compilation).
        let nested = ports
            .process()
            .execute(&sh(
                "/usr/bin/sandbox-exec -p '(version 1)(allow default)' /usr/bin/true".to_owned(),
            ))
            .await;
        let nested_text = match nested {
            Err(ego_core::permit::ProcessError::Exit { result, .. }) => result
                .content
                .iter()
                .map(|block| block.text.clone())
                .collect::<String>(),
            other => panic!("a nested sandbox must be refused: {other:?}"),
        };
        assert!(
            nested_text.contains("start its own sandbox")
                && nested_text.contains("--disable-sandbox"),
            "{nested_text}"
        );
        // Catches: git signing with a private key path reported as a bare ENOENT/EPERM.
        let signing = ports
            .process()
            .execute(&sh("echo 'No private key found for \"/x/id\"?' >&2; exit 1".to_owned()))
            .await;
        let signing_text = match signing {
            Err(ego_core::permit::ProcessError::Exit { result, .. }) => result
                .content
                .iter()
                .map(|block| block.text.clone())
                .collect::<String>(),
            other => panic!("must fail: {other:?}"),
        };
        assert!(signing_text.contains("user.signingkey") && signing_text.contains(".pub"), "{signing_text}");
        // Catches: under workspace the child keeps a TMPDIR outside the sandbox,
        // so mktemp, cargo and mbx fail on a path they cannot write.
        if level == Confinement::Workspace {
            let scratch = ports
                .process()
                .execute(&sh("test \"$TMPDIR\" != \"$EGO_DEV_OUTSIDE_TMP\" && mkdir \"$TMPDIR/egodev\" && rmdir \"$TMPDIR/egodev\"".to_owned()))
                .await;
            assert!(scratch.is_ok(), "workspace TMPDIR must be writable: {scratch:?}");
        }
        // Catches: the derived cache root reaching the rest of HOME.
        assert!(!ok(format!("echo x > '{}'", home.join("other/new").display())).await);
    });
    served.join().unwrap();
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn adaptive_reaches_public_keys_and_the_mbx_cache() {
    isolated("adaptive_reaches_public_keys_and_the_mbx_cache", "adaptive");
}

#[test]
fn workspace_reaches_public_keys_and_the_mbx_cache() {
    isolated(
        "workspace_reaches_public_keys_and_the_mbx_cache",
        "workspace",
    );
}

#[test]
fn workspace_git_status_and_commit_work_in_a_linked_worktree() {
    isolated(
        "workspace_git_status_and_commit_work_in_a_linked_worktree",
        "workspace",
    );
}
