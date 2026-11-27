//! Native regressions for credential exposure through adaptive admission.

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

fn isolated_case(name: &str, paths: &[&str]) {
    if std::env::var("EGO_CRITIC_CASE").as_deref() != Ok(name) {
        let root = tempfile::Builder::new()
            .prefix(".ego-critic-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        for dir in ["home", "workspace", "tmp", "cargo", "rustup", "ego"] {
            std::fs::create_dir(root.path().join(dir)).unwrap();
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("adaptive_critic::{name}"),
                "--nocapture",
            ])
            .env("EGO_CRITIC_CASE", name)
            .env("EGO_CRITIC_ROOT", root.path())
            .env("HOME", root.path().join("home"))
            .env("TMPDIR", root.path().join("tmp"))
            .env("CARGO_HOME", root.path().join("cargo"))
            .env("RUSTUP_HOME", root.path().join("rustup"))
            .env("EGO_HOME", root.path().join("ego"))
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated credential probe failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let root = std::env::var_os("EGO_CRITIC_ROOT").unwrap();
    let root = Path::new(&root);
    let home = root.join("home");
    // Only synthetic stores under the task-owned HOME are ever opened.
    for path in paths
        .iter()
        .copied()
        .chain([".ssh/synthetic", "instruction.txt"])
    {
        let path = home.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"synthetic-credential-probe").unwrap();
    }
    let policy = admit_confinement(
        Confinement::Adaptive,
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
        let read = |path: &str| {
            SpawnSpec::new(
                vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "exec /bin/cat \"$HOME/$1\"".into(),
                    "ego-critic".into(),
                    path.into(),
                ],
                None,
                Some(Duration::from_secs(5)),
                EnvPolicy::Inherited,
            )
            .unwrap()
        };
        let ordinary = ports.process().execute(&read("instruction.txt")).await;
        assert!(ordinary.is_ok(), "ordinary read control: {ordinary:?}");
        let ssh = ports.process().execute(&read(".ssh/synthetic")).await;
        assert!(ssh.is_err(), "SSH denial control unexpectedly succeeded");
        let mut exposed = Vec::new();
        for path in paths {
            if ports.process().execute(&read(path)).await.is_ok() {
                exposed.push(*path);
            }
        }
        assert!(
            exposed.is_empty(),
            "adaptive leaked credential stores: {exposed:?}"
        );
    });
}

// Catches: broad adaptive reads expose standard Git and GitHub credential stores.
#[test]
fn adaptive_denies_git_and_github_credential_stores() {
    isolated_case(
        "adaptive_denies_git_and_github_credential_stores",
        &[".git-credentials", ".config/gh/hosts.yml"],
    );
}

// Catches: relocating CARGO_HOME removes the floor from an existing default token store.
#[test]
fn adaptive_denies_default_cargo_credentials_after_cargo_home_moves() {
    isolated_case(
        "adaptive_denies_default_cargo_credentials_after_cargo_home_moves",
        &[".cargo/credentials.toml"],
    );
}
