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
        registry.classify(registry.submit("/config perimeter", InputOrigin::Typed)),
        Dispatch::Action(ActionId::Config)
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

// Catches: redundant roots are presented without warning, including through a symlink.
#[cfg(unix)]
#[test]
fn overlapping_perimeter_roots_warn_without_changing_admitted_access() {
    let scratch = tempfile::tempdir().unwrap();
    let workspace = scratch.path().join("workspace");
    let home = scratch.path().join("ego-home");
    let nested = workspace.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir(&home).unwrap();
    let alias = scratch.path().join("alias");
    std::os::unix::fs::symlink(&nested, &alias).unwrap();
    std::fs::write(home.join("config.toml"), format!(
        "[profile.inspect]\nroots = [{{path = '{}', access = 'read-write'}}, {{path = '{}', access = 'read'}}]\n",
        workspace.display(), alias.display()
    )).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ego"))
        .args(["config", "ls", "--effective", "--json"])
        .current_dir(&workspace)
        .env("EGO_HOME", &home)
        .env("EGO_PROFILE", "inspect")
        .env_remove("EGO_MODEL")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let warnings = value["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 3);
    assert!(warnings.iter().any(|warning| {
        warning
            .as_str()
            .unwrap()
            .contains("covered by a read-write root")
    }));
    assert_eq!(
        value["roots"][1]["path"],
        nested.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(value["roots"][1]["access"], "read");
}
