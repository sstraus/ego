//! Real-process regressions for the operator configuration write floor: a
//! hardlinked configuration file or a writable root that contains the ego
//! configuration must not refuse the session, and must not become a way to
//! change the configuration.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::path::{Path, PathBuf};
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

const ORIGINAL: &[u8] = b"mode = 'default'\n";

/// Re-run `name` in a child whose `EGO_HOME` is `<root>/<home>`; `workspace/ego`
/// puts the operator configuration inside the writable workspace.
fn isolated(name: &str, home: &str, body: fn(&Path)) {
    if std::env::var("EGO_FLOOR_CASE").as_deref() == Ok(name) {
        body(Path::new(&std::env::var_os("EGO_FLOOR_ROOT").unwrap()));
        return;
    }
    let root = tempfile::Builder::new()
        .prefix(".ego-floor-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    for dir in [
        "workspace",
        "workspace/ego",
        "workspace/other",
        "workspace/clean",
        "outside",
        "tmp",
        "home",
    ] {
        std::fs::create_dir(root.path().join(dir)).unwrap();
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("EGO_FLOOR_CASE", name)
        .env("EGO_FLOOR_ROOT", root.path())
        .env("HOME", root.path().join("home"))
        .env("TMPDIR", root.path().join("tmp"))
        .env("EGO_HOME", root.path().join(home))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated case failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_shell(
    ports: &RunEffectPorts,
    runtime: &tokio::runtime::Runtime,
    script: &str,
    argument: &Path,
) -> bool {
    let spec = SpawnSpec::new(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            script.into(),
            "ego-floor".into(),
            argument.into(),
        ],
        None,
        Some(Duration::from_secs(10)),
        EnvPolicy::Inherited,
    )
    .unwrap();
    runtime.block_on(ports.process().execute(&spec)).is_ok()
}

fn session(
    root: &Path,
) -> (
    tokio::runtime::Runtime,
    Result<RunEffectPorts, ego_core::permit::ProcessError>,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ports = runtime.block_on(async {
        let policy = admit_confinement(
            Confinement::Workspace,
            false,
            CanonPath::new(std::fs::canonicalize(root.join("workspace")).unwrap()),
        )?;
        let evidence =
            SessionEvidenceStore::open(root.join("evidence"), SessionId::new(), 1024 * 1024)
                .unwrap();
        let config = ExecRunConfig::new(
            policy,
            evidence,
            CancelToken::new(),
            ExecLimits::default(),
            HttpCallTimeout::new(Duration::from_secs(1)).unwrap(),
            None,
            Arc::new(SystemResolver::new()),
        );
        RunEffectPorts::for_run(config)
            .await
            .map(|(ports, _, _)| ports)
    });
    (runtime, ports)
}

fn config_with_alias(root: &Path) -> (PathBuf, PathBuf) {
    let config = root.join("workspace/ego/config.toml");
    std::fs::write(&config, ORIGINAL).unwrap();
    let alias = root.join("workspace/other/alias");
    std::fs::hard_link(&config, &alias).unwrap();
    (config, alias)
}

fn hardlinked_overlapping_case(root: &Path) {
    let (config, _alias) = config_with_alias(root);
    let (runtime, ports) = session(root);
    let ports = ports.unwrap_or_else(|error| panic!("session refused: {error}"));
    let unrelated = root.join("workspace/clean/unrelated.txt");
    assert!(
        run_shell(&ports, &runtime, "printf done > \"$1\"", &unrelated),
        "unrelated workspace write was denied"
    );
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"done");
    assert_eq!(std::fs::read(&config).unwrap(), ORIGINAL);
}

// Catches: the owner cannot start ordinary work because a config hardlink or
// broad root overlaps config.
#[test]
fn hardlinked_or_overlapping_config_does_not_block_unrelated_work() {
    isolated(
        "hardlinked_or_overlapping_config_does_not_block_unrelated_work",
        "workspace/ego",
        hardlinked_overlapping_case,
    );
}

fn self_grant_case(root: &Path) {
    let (config, alias) = config_with_alias(root);
    let (runtime, ports) = session(root);
    let ports = ports.unwrap_or_else(|error| panic!("session refused: {error}"));
    let overwrite = "printf compromised > \"$1\"";
    for (label, target) in [("direct path", &config), ("pre-existing hardlink", &alias)] {
        assert!(
            !run_shell(&ports, &runtime, overwrite, target),
            "{label} write was admitted"
        );
    }
    // Creating a new name for the inode, then writing through it, must not
    // change the configuration whether or not the link itself is admitted.
    let created = root.join("workspace/other/created-alias");
    let link_then_write = format!(
        "ln \"$1\" '{0}' && printf compromised > '{0}'",
        created.display()
    );
    let _ = run_shell(&ports, &runtime, &link_then_write, &config);
    assert!(
        !run_shell(&ports, &runtime, "rm -f \"$1\"", &config),
        "config removal was admitted"
    );
    assert_eq!(std::fs::read(&config).unwrap(), ORIGINAL);
    assert_eq!(std::fs::read(&alias).unwrap(), ORIGINAL);
}

// Catches: a compromised dependency edits ego config through a broad root or
// hardlink to self-authorize.
#[test]
fn unapproved_config_write_cannot_self_grant_authority() {
    isolated(
        "unapproved_config_write_cannot_self_grant_authority",
        "workspace/ego",
        self_grant_case,
    );
}

// Catches: the fixture itself cannot admit the workspace, which would make the
// overlap cases above fail for a reason unrelated to the configuration.
#[test]
fn workspace_with_config_elsewhere_runs_unrelated_work() {
    isolated(
        "workspace_with_config_elsewhere_runs_unrelated_work",
        "outside",
        |root| {
            let (runtime, ports) = session(root);
            let ports = ports.unwrap_or_else(|error| panic!("session refused: {error}"));
            let unrelated = root.join("workspace/new.txt");
            assert!(run_shell(
                &ports,
                &runtime,
                "printf done > \"$1\"",
                &unrelated
            ));
            assert_eq!(std::fs::read(&unrelated).unwrap(), b"done");
        },
    );
}

/// A libtest host of the Landlock helper: without this entry point the probe
/// children run unconfined and admission cannot demonstrate any guarantee.
/// Success never returns; the helper replaces this process with the target.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "hosts the Landlock helper; not a test of its own"]
fn __ego_internal_landlock_exec_v1() {
    let refused = ego_tools::exec::dispatch_landlock_helper(std::env::args_os())
        .expect("libtest was handed the helper envelope but did not recognise it");
    panic!("the Landlock helper refused execution: {refused:?}");
}
