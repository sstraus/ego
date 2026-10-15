//! A nested CLI inherits the production process sandbox, not host apply authority.
#![cfg(target_os = "macos")]

use ego_core::event::id::SessionId;
use ego_core::permit::{CanonPath, Confinement, EnvPolicy, RunConfinement, SpawnSpec};
use ego_core::turn::CancelToken;
use ego_tools::exec::{
    ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts, SessionEvidenceStore,
};
use ego_tools::http::SystemResolver;
use std::sync::Arc;
use std::time::Duration;

// Catches: nested setup treats model/process input as interactive apply authority.
#[test]
fn setup_nested_cli_inherits_config_floor_and_reports_only() {
    if std::env::var_os("EGO_SETUP_NESTED_CASE").is_none() {
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "setup_nested_cli_inherits_config_floor_and_reports_only",
            ])
            .env("EGO_SETUP_NESTED_CASE", "1")
            .env("EGO_HOME", root.path().join("home"))
            .env("HOME", root.path().join("home"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let home = std::path::PathBuf::from(std::env::var_os("EGO_HOME").unwrap());
    std::fs::create_dir_all(&home).unwrap();
    let original = "sandbox='workspace'\nroots=[]\n";
    std::fs::write(home.join("config.toml"), original).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let evidence = tempfile::tempdir().unwrap();
    let cwd = CanonPath::new(std::fs::canonicalize(workspace.path()).unwrap());
    let binary = std::fs::canonicalize(env!("CARGO_BIN_EXE_ego")).unwrap();
    let policy = RunConfinement::new(Confinement::Workspace, false, cwd.clone())
        .unwrap()
        .with_exec_paths(vec![
            CanonPath::new(binary.parent().unwrap()),
            CanonPath::new(&home),
        ]);
    let store = SessionEvidenceStore::open(evidence.path(), SessionId::new(), 1024 * 1024).unwrap();
    let config = ExecRunConfig::new(
        policy,
        store,
        CancelToken::new(),
        ExecLimits::new(64 * 1024, Duration::from_millis(250)).unwrap(),
        HttpCallTimeout::default(),
        None,
        Arc::new(SystemResolver::new()),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (ports, _, _) = runtime.block_on(RunEffectPorts::for_run(config)).unwrap();
    let spec = SpawnSpec::new(
        vec![binary.into_os_string(), "setup".into()],
        Some(cwd.clone()),
        Some(Duration::from_secs(10)),
        EnvPolicy::Inherited,
    )
    .unwrap();
    let result = runtime.block_on(ports.process().execute(&spec)).unwrap();
    let output = result
        .content
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        output.contains("Report only; no human apply authority"),
        "{output}"
    );
    let overwrite = format!("printf forged > '{}'", home.join("config.toml").display());
    let spec = SpawnSpec::new(
        vec!["/bin/sh".into(), "-c".into(), overwrite.into()],
        Some(cwd),
        Some(Duration::from_secs(10)),
        EnvPolicy::Inherited,
    )
    .unwrap();
    let _ = runtime.block_on(ports.process().execute(&spec));
    assert_eq!(
        std::fs::read_to_string(home.join("config.toml")).unwrap(),
        original
    );
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 0);
}
