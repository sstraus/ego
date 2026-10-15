//! Real binary setup uses a scripted local provider and no ordinary run store.
mod support;
use support::{Plan, Reply, Stub, text};

fn setup(response: Option<&str>) -> (String, tempfile::TempDir, tempfile::TempDir, Option<Stub>) {
    setup_budget(response, None)
}

fn setup_budget(
    response: Option<&str>,
    budget: Option<&str>,
) -> (String, tempfile::TempDir, tempfile::TempDir, Option<Stub>) {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("AGENTS.md"), "fixture instructions\n").unwrap();
    let stub = response.map(|response| {
        Stub::start(Plan {
            completions: vec![Reply::Sse(if budget.is_some() {
                support::costing(response, 1000, 1)
            } else {
                text(response)
            })],
            ..Plan::default()
        })
    });
    if stub.is_some() {
        std::fs::write(
            home.path().join("config.toml"),
            format!(
                "model='ollama/qwen3'\nsandbox='off'\nroots=[]\n{}",
                budget
                    .map(|budget| format!("budget='{budget}'\n"))
                    .unwrap_or_default()
            ),
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
    assert!(
        report.contains("invalid closed probe selection"),
        "{report}"
    );
    assert!(report.contains("AGENTS.md"), "{report}");
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 1);
}

// Catches: missing provider triggers bootstrap, fabricated NLP or durable session state.
#[test]
fn setup_without_provider_reports_prompt_unknown_and_inventory_without_writes() {
    let (report, home, workspace, _) = setup(None);
    assert!(report.contains("no configured model"), "{report}");
    assert!(report.contains("interpretation is unknown"), "{report}");
    assert!(report.contains("AGENTS.md"), "{report}");
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 1);
}

// Catches: setup bypasses the host signal owner while provider discovery is stalled.
#[cfg(unix)]
#[test]
fn setup_provider_sigint_returns_cancelled_without_store_writes() {
    use std::time::{Duration, Instant};
    let gate = support::ReleaseGate::new();
    let stub = Stub::start(Plan {
        discovery_gate: Some(gate.clone()),
        ..Plan::default()
    });
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = "model='ollama/qwen3'\nsandbox='off'\nroots=[]\n";
    std::fs::write(home.path().join("config.toml"), config).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_ego"))
        .env_clear()
        .env("EGO_HOME", home.path())
        .env("HOME", home.path())
        .env("PATH", support::fixture_path())
        .env("OLLAMA_HOST", &stub.address)
        .current_dir(workspace.path())
        .args(["setup", "inspect work"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !stub
        .requests()
        .iter()
        .any(|request| request.path == "/api/tags")
    {
        assert!(
            Instant::now() < deadline,
            "setup did not reach provider discovery"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        std::process::Command::new("/bin/kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    gate.release();
    if status.is_none() {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(130),
        "setup did not return cancellation exit; stderr {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
        config
    );
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 0);
}

// Catches: setup ignores the resolved token ceiling at its generation boundaries.
#[test]
fn setup_provider_token_ceiling_binds_without_rates_or_store_writes() {
    let response = r#"[{"kind":"tool","name":"token-bound-tool","reason":"need"}]"#;
    for (budget, inferred, requests) in [("0k", false, 0), ("1k", false, 1), ("2k", true, 1)] {
        let (report, home, workspace, stub) = setup_budget(Some(response), Some(budget));
        assert_eq!(
            report.contains("Configured provider inference"),
            inferred,
            "{budget}: {report}"
        );
        assert_eq!(stub.unwrap().generations().len(), requests, "{budget}");
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 1);
    }
}

// Catches: setup discards a valid closed response from the hosted Responses path.
#[test]
fn setup_openai_valid_closed_response_selects_probes() {
    let selection = r#"[{"kind":"tool","name":"hosted-setup-tool","reason":"requested"}]"#;
    let delta = serde_json::json!({"type":"response.output_text.delta", "delta":selection});
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(format!(
            "data: {delta}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\",\"usage\":{{\"input_tokens\":20,\"output_tokens\":30}}}}}}\n\n"
        ))],
        ..Plan::default()
    });
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "model='openai/gpt-6-luna'\nsandbox='off'\nroots=[]\n",
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ego"))
        .env_clear()
        .env("EGO_HOME", home.path())
        .env("HOME", home.path())
        .env("PATH", support::fixture_path())
        .env("OPENAI_API_KEY", "fixture-key")
        .env("OPENAI_BASE_URL", format!("http://{}", stub.address))
        .current_dir(workspace.path())
        .args(["setup", "inspect the work"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.contains("Configured provider inference"), "{report}");
    assert!(report.contains("hosted-setup-tool"), "{report}");
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 1);
}
