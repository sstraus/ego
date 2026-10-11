use std::process::Command;

// Catches: config prints stored values while doctor omits or redacts admitted roots.
#[test]
fn effective_perimeter_is_shared_without_widening_doctor_redaction() {
    let scratch = tempfile::tempdir().unwrap();
    let workspace = scratch.path().join("workspace");
    let home = scratch.path().join("ego-home");
    let extra = scratch.path().join("admitted root");
    for path in [&workspace, &home, &extra] {
        std::fs::create_dir(path).unwrap();
    }
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[profile.inspect]\nsandbox = 'ro'\nroots = [{{path = '{}', access = 'read'}}]\n",
            extra.display()
        ),
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ego"))
            .args(args)
            .current_dir(&workspace)
            .env("EGO_HOME", &home)
            .env("EGO_PROFILE", "inspect")
            .env_remove("EGO_MODEL")
            .output()
            .unwrap()
    };
    let config = run(&["config", "ls", "--effective"]);
    assert!(
        config.status.success(),
        "{}",
        String::from_utf8_lossy(&config.stderr)
    );
    let block = String::from_utf8(config.stdout).unwrap();
    assert!(block.starts_with("perimeter:\n"));
    assert!(block.contains(&format!(
        "workspace\t{}",
        workspace.canonicalize().unwrap().display()
    )));
    assert!(block.contains(&format!(
        "root\t{}\tread\tuser",
        extra.canonicalize().unwrap().display()
    )));
    assert!(block.contains("sandbox\tro\tuser"));
    assert!(block.contains("capabilities\tnot_checked"));
    assert!(block.contains("network\tonline"));
    let doctor = run(&["doctor"]);
    assert!(
        String::from_utf8(doctor.stdout)
            .unwrap()
            .contains(block.trim_end())
    );
    let json = run(&["config", "ls", "--effective", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["capabilities"], "not_checked");
    assert_eq!(
        value["capabilities_reason"],
        "no completed backend measurement is available; inspection does not run probes"
    );
    assert!(value.get("probe_evidence").is_none());
    assert_eq!(value["sandbox"], "ro");
    assert_eq!(value["roots"][0]["source"], "user");
    let visible = ego_cli::doctor::visible_roots(&workspace, Some(&home));
    assert!(
        ego_core::health::HealthDetail::sanitized(
            "other.diagnostic",
            extra.to_string_lossy(),
            &visible
        )
        .is_err(),
        "the perimeter exception must not admit paths for other diagnostics"
    );
}

// Catches: /config perimeter is sent to the model or accepts unknown subcommands.
#[test]
fn slash_perimeter_is_local_and_rejects_unknown_arguments() {
    use ego_cli::repl::actions::{ActionId, ActionRegistry, Dispatch, InputOrigin};
    let registry = ActionRegistry::final_surface();
    assert!(matches!(
        registry.classify(registry.submit("/config", InputOrigin::Typed)),
        Dispatch::Action(ActionId::Config)
    ));
    assert!(
        matches!(registry.classify(registry.submit("/config set mode 'plan'", InputOrigin::Typed)), Dispatch::Config(arguments) if arguments == "set mode 'plan'")
    );
    assert!(matches!(
        registry.classify(registry.submit("/config set mode 'plan'", InputOrigin::BracketedPaste)),
        Dispatch::Prompt
    ));
    assert!(matches!(
        registry.classify(registry.submit("/config perimeter", InputOrigin::Typed)),
        Dispatch::Config(arguments) if arguments == "perimeter"
    ));
    assert!(matches!(
        registry.classify(registry.submit("/config unknown", InputOrigin::Typed)),
        Dispatch::Refused { .. }
    ));
}

// Catches: inspection reconstructs an online/default run instead of projecting the live session.
#[test]
fn session_perimeter_retains_offline_confinement_and_canonical_roots() {
    use ego_core::config::{ConfigLayer, Source, Trust, resolve};
    use ego_core::permit::{CanonPath, Confinement, RunConfinement};
    let scratch = tempfile::tempdir().unwrap();
    let workspace = scratch.path().canonicalize().unwrap();
    let layer = ConfigLayer::parse(Source::User, "sandbox = 'ro'").unwrap();
    let effective = resolve(&[layer], Trust::Untrusted, None).unwrap();
    let confinement =
        RunConfinement::new(Confinement::Ro, true, CanonPath::new(&workspace)).unwrap();
    let perimeter = ego_cli::perimeter::Perimeter::session(&confinement, &effective, &[]);
    let block = perimeter.text();
    assert!(block.contains("sandbox\tro\tuser"));
    assert!(block.contains("network\toffline\tinvocation"));
    assert!(block.contains("roots\tnone\tdefault"));
    assert!(
        ego_cli::doctor::observed_session(workspace, 1000, &perimeter)
            .contains("network  offline  invocation")
    );
}

// Catches: a refused root is printed as admitted or leaks through doctor error details.
#[test]
fn unadmitted_perimeter_fails_closed_without_raw_doctor_paths() {
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("ego-home");
    std::fs::create_dir(&home).unwrap();
    let missing = scratch.path().join("private-missing-root");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[profile.inspect]\nroots = [{{path = '{}', access = 'read'}}]\n",
            missing.display()
        ),
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ego"))
            .args(args)
            .current_dir(scratch.path())
            .env("EGO_HOME", &home)
            .env("EGO_PROFILE", "inspect")
            .env_remove("EGO_MODEL")
            .output()
            .unwrap()
    };
    let inspection = run(&["config", "ls", "--effective"]);
    assert!(!inspection.status.success());
    let warning = String::from_utf8(inspection.stderr).unwrap();
    assert!(warning.contains("warning: configured perimeter roots"));
    assert!(warning.contains("cannot be resolved"));
    let doctor = run(&["doctor", "--json"]);
    assert!(!doctor.status.success());
    let output = String::from_utf8(doctor.stdout).unwrap();
    assert!(output.contains("the effective perimeter could not be resolved or admitted"));
    assert!(!output.contains(missing.to_str().unwrap()));
}

// Catches: a canonical alias bypasses nested-read-only refusal; workspace overlap stays visible.
#[cfg(unix)]
#[test]
fn overlapping_roots_refuse_read_nesting_and_warn_on_workspace_overlap() {
    let scratch = tempfile::tempdir().unwrap();
    let workspace = scratch.path().join("workspace");
    let home = scratch.path().join("ego-home");
    let nested = workspace.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir(&home).unwrap();
    let alias = scratch.path().join("alias");
    std::os::unix::fs::symlink(&nested, &alias).unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_ego"))
            .args(["config", "ls", "--effective", "--json"])
            .current_dir(&workspace)
            .env("EGO_HOME", &home)
            // Adaptive refuses a workspace inside `$TMPDIR`, where `tempdir` puts it.
            .env("TMPDIR", scratch.path().join("tmp"))
            .env("EGO_PROFILE", "inspect")
            .env_remove("EGO_MODEL")
            .output()
            .unwrap()
    };
    let config = |access| {
        format!(
            "[profile.inspect]\nroots=[{{path='{}',access='read-write'}},{{path='{}',access='{access}'}}]\n",
            workspace.display(),
            alias.display()
        )
    };
    std::fs::write(home.join("config.toml"), config("read")).unwrap();
    let refused = run();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("nested-read-only-root"));
    std::fs::write(home.join("config.toml"), config("read-write")).unwrap();
    let output = run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap().contains("overlaps the workspace"))
    );
    assert_eq!(
        value["roots"][1]["path"],
        nested.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(value["roots"][1]["access"], "read-write");
}

// Catches: session inspection hides a real measurement or substitutes the mechanism's promised caps.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn session_perimeter_projects_existing_probe_evidence_without_remeasuring() {
    use ego_core::config::{Trust, resolve};
    use ego_core::event::id::SessionId;
    use ego_core::permit::{CanonPath, Confinement, ExecCaps, ExecObservation, RunConfinement};
    use ego_core::turn::CancelToken;
    use ego_tools::exec::{ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts};
    use ego_tools::http::SystemResolver;
    use ego_tools::store::SessionEvidenceStore;
    use std::sync::Arc;

    let scratch = tempfile::tempdir().unwrap();
    let workspace = scratch.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let confinement =
        RunConfinement::new(Confinement::Ro, false, CanonPath::new(&workspace)).unwrap();
    let effective = resolve(&[], Trust::Untrusted, None).unwrap();
    let evidence =
        SessionEvidenceStore::open(scratch.path(), SessionId::new(), 1024 * 1024).unwrap();
    let config = ExecRunConfig::new(
        confinement.clone(),
        evidence,
        CancelToken::new(),
        ExecLimits::default(),
        HttpCallTimeout::default(),
        None,
        Arc::new(SystemResolver::new()),
    );
    let (observation, transcript) = RunEffectPorts::observe(&config).await.unwrap();
    let transcript =
        transcript.expect("the real paired suite must retain its measurement transcript");
    let perimeter = ego_cli::perimeter::Perimeter::session(&confinement, &effective, &[])
        .with_execution_observation(&observation, Some(&transcript));
    let value = serde_json::to_value(&perimeter).unwrap();
    assert_eq!(
        value["capabilities"],
        serde_json::to_value(observation.capabilities()).unwrap()
    );
    assert_eq!(value["backend"], observation.backend().name());
    assert_eq!(
        value["probe_evidence"],
        serde_json::to_value(&transcript).unwrap()
    );
    assert!(value.get("capabilities_reason").is_none());
    assert!(!perimeter.text().contains("not_checked"));
    let doctor = ego_cli::doctor::observed_session(workspace.clone(), 1000, &perimeter);
    assert!(
        doctor.contains("the existing execution backend measurement"),
        "{doctor}"
    );
    assert!(!doctor.contains("which doctor does not do"), "{doctor}");

    // Exercise an empty domain observation at the projection boundary. This
    // does not claim that the host's OS produced an empty measurement.
    let empty = ExecObservation::new(
        observation.backend().clone(),
        ExecCaps::empty(),
        observation.policy_digest().clone(),
    );
    let empty_view = perimeter.with_execution_observation(&empty, Some(&transcript));
    let value = serde_json::to_value(&empty_view).unwrap();
    assert_eq!(value["capabilities"], serde_json::json!([]));
    assert!(empty_view.text().contains("no confinement guarantee"));
    assert!(!empty_view.text().contains("not_checked"));

    let unmeasured = empty_view.with_execution_observation(&observation, None);
    let value = serde_json::to_value(unmeasured).unwrap();
    assert_eq!(value["capabilities"], "not_checked");
    assert_eq!(
        value["capabilities_reason"],
        "the selected backend ran no capability probes"
    );
    assert!(value.get("probe_evidence").is_none());
}
