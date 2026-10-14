//! Real binary setup uses a scripted local provider and no ordinary run store.
mod support;
use support::{Plan, Reply, Stub, text};

fn setup(response: Option<&str>) -> (String, tempfile::TempDir, tempfile::TempDir, Option<Stub>) {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("AGENTS.md"), "fixture instructions\n").unwrap();
    let stub = response.map(|response| {
        Stub::start(Plan {
            completions: vec![Reply::Sse(text(response))],
            ..Plan::default()
        })
    });
    if stub.is_some() {
        std::fs::write(
            home.path().join("config.toml"),
            "model='ollama/qwen3'\nsandbox='off'\nroots=[]\n",
        )
        .unwrap();
    }
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ego"));
    command
        .env_clear()
        .env("EGO_HOME", home.path())
        .env("HOME", home.path())
        .env("PATH", support::fixture_path())
        .current_dir(workspace.path())
        .args(["setup", "inspect the work"]);
    if let Some(stub) = &stub {
        command.env("OLLAMA_HOST", &stub.address);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        String::from_utf8(output.stdout).unwrap(),
        home,
        workspace,
        stub,
    )
}

// Catches: a configured prompt is ignored or generates ordinary tools/history.
#[test]
fn setup_configured_model_selects_only_closed_probes_without_store_writes() {
    let (report, home, workspace, stub) = setup(Some(
        r#"[{"kind":"tool","name":"missing-setup-tool","reason":"user need"}]"#,
    ));
    assert!(report.contains("Configured provider inference"), "{report}");
    assert!(report.contains("missing-setup-tool"), "{report}");
    let generations = stub.unwrap().generations();
    assert_eq!(generations.len(), 1);
    assert!(
        generations[0]
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
    );
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 1);
}

// Catches: unsupported provider text reaches a process or config writer.
#[test]
fn setup_unsupported_model_operation_falls_back_without_effects() {
    let (report, home, workspace, _) = setup(Some(r#"[{"kind":"exec","command":"touch forged"}]"#));
    assert!(report.contains("interpretation is unknown"), "{report}");
    assert!(report.contains("AGENTS.md"), "{report}");
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 1);
}

// Catches: missing provider triggers bootstrap, fabricated NLP or durable session state.
#[test]
fn setup_without_provider_reports_prompt_unknown_and_inventory_without_writes() {
    let (report, home, workspace, _) = setup(None);
    assert!(report.contains("interpretation is unknown"), "{report}");
    assert!(report.contains("AGENTS.md"), "{report}");
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 1);
}
