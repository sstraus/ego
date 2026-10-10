#[cfg(target_os = "macos")]
mod native {
    use super::super::*;
    use crate::exec::{
        ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts, SessionEvidenceStore,
    };
    use ego_core::event::id::SessionId;
    use ego_core::permit::{EnvPolicy, SpawnSpec};
    use ego_core::turn::CancelToken;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::time::Duration;

    fn execute(
        ports: &RunEffectPorts,
        workspace: &CanonPath,
        argv: Vec<std::ffi::OsString>,
    ) -> Result<ego_core::permit::ToolResult, ProcessError> {
        let spec = SpawnSpec::new(
            argv,
            Some(workspace.clone()),
            Some(Duration::from_secs(30)),
            EnvPolicy::Inherited,
        )
        .unwrap();
        crate::exec::runtime_handle().block_on(ports.process().execute(&spec))
    }

    // Catches: broad reads/execution still hit old walls, derived writes are incomplete,
    // credential/control exclusions are overridden, or writable build artifacts cannot run.
    #[test]
    fn adaptive_native_reach_preserves_floors_and_runs_rustc_generated_tests() {
        let root = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(root.path()).unwrap();
        let home = base.join("home");
        let workspace = base.join("workspace");
        let names = ["cargo", "rustup", "mbx", "git-common", "tmp"];
        let writable = names.map(|name| base.join(name));
        for path in writable.iter().chain([&home, &workspace]) {
            std::fs::create_dir(path).unwrap();
        }
        let credentials = home.join(".ssh");
        std::fs::create_dir(&credentials).unwrap();
        let credential = credentials.join("synthetic");
        std::fs::write(&credential, b"synthetic credential").unwrap();
        let alias = base.join("credential-alias");
        std::os::unix::fs::symlink(&credential, &alias).unwrap();
        let instruction = home.join("instruction.md");
        std::fs::write(&instruction, b"instruction").unwrap();
        let outside_env = home.join(".env.fixture");
        std::fs::write(&outside_env, b"synthetic env").unwrap();
        let control = writable[3].join("hooks");
        std::fs::create_dir(&control).unwrap();
        let perimeter = capture(
            &home,
            &workspace,
            writable.to_vec(),
            vec![credentials],
            vec![control.clone()],
        )
        .unwrap();
        let workspace = CanonPath::new(workspace);
        let policy = RunConfinement::new(Confinement::Adaptive, false, workspace.clone())
            .unwrap()
            .with_adaptive_perimeter(perimeter);
        let evidence =
            SessionEvidenceStore::open(&base.join("evidence"), SessionId::new(), 1024 * 1024)
                .unwrap();
        let config = ExecRunConfig::new(
            policy.clone(),
            evidence,
            CancelToken::new(),
            ExecLimits::default(),
            HttpCallTimeout::default(),
            None,
            Arc::new(crate::http::SystemResolver::new()),
        );
        let (ports, observation, _) = crate::exec::runtime_handle()
            .block_on(RunEffectPorts::for_run(config))
            .unwrap_or_else(|error| {
                let measurement = crate::exec::runtime_handle()
                    .block_on(RunEffectPorts::measure_raw(
                        &policy,
                        CancelToken::new(),
                        ExecLimits::default(),
                    ))
                    .expect("cached probe transcript");
                let records = measurement
                    .probes
                    .snapshot()
                    .iter()
                    .map(|record| format!("{}: {}", record.suite, record.outcome))
                    .collect::<Vec<_>>();
                panic!("{error}: {records:?}");
            });
        assert!(
            observation
                .capabilities()
                .contains(ego_core::permit::ExecCaps::ADAPTIVE)
        );
        let cat = |path: &Path| {
            execute(
                &ports,
                &workspace,
                vec!["/bin/cat".into(), path.as_os_str().to_owned()],
            )
        };
        assert!(cat(&instruction).is_ok());
        for path in [&credential, &alias, &outside_env] {
            assert!(cat(path).is_err(), "read allowed: {}", path.display());
        }
        for path in &writable {
            let output = path.join("created");
            execute(
                &ports,
                &workspace,
                vec!["/usr/bin/touch".into(), output.clone().into_os_string()],
            )
            .unwrap();
            assert!(output.is_file());
        }
        for path in [home.join("outside"), control.join("pre-commit")] {
            assert!(
                execute(
                    &ports,
                    &workspace,
                    vec!["/usr/bin/touch".into(), path.clone().into_os_string()]
                )
                .is_err()
            );
            assert!(!path.exists());
        }
        let installed = home.join("installed-tool");
        std::fs::write(&installed, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).unwrap();
        execute(&ports, &workspace, vec![installed.into_os_string()]).unwrap();
        let output = std::process::Command::new("rustup")
            .args(["which", "rustc"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let rustc = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        execute(
            &ports,
            &workspace,
            vec![rustc.clone().into_os_string(), "--version".into()],
        )
        .unwrap();
        let target = workspace.as_path().join("target");
        std::fs::create_dir(&target).unwrap();
        let source = workspace.as_path().join("fixture.rs");
        std::fs::write(
            &source,
            b"#[test] fn generated() { assert_eq!(2 + 2, 4); }\n",
        )
        .unwrap();
        let binary = target.join("fixture-test");
        execute(
            &ports,
            &workspace,
            vec![
                rustc.into_os_string(),
                "--test".into(),
                source.into_os_string(),
                "-o".into(),
                binary.clone().into_os_string(),
            ],
        )
        .unwrap();
        execute(&ports, &workspace, vec![binary.into_os_string()]).unwrap();
    }
}
