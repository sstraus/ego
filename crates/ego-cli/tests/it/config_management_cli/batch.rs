use super::*;

// Catches: earlier keys published before a later validation failure.
#[test]
fn managed_batch_invalid_last_operation_writes_nothing() {
    let fixture = Fixture::new();
    fixture.write_config("# unchanged\nmodel='ollama/original'\n");
    let accepted = fixture.run(&[
        "config",
        "batch",
        "--set",
        "model='ollama/accepted'",
        "--set",
        "mode='plan'",
    ]);
    assert!(accepted.status.success(), "{}", error(&accepted));
    for (key, expected) in [("model", "\"ollama/accepted\"\n"), ("mode", "\"plan\"\n")] {
        let readback = fixture.run(&["config", "get", key]);
        assert!(readback.status.success(), "{}", error(&readback));
        assert_eq!(text(&readback), expected);
    }
    let before = std::fs::read(fixture.config_path()).unwrap();
    for tail in [
        "bare='not a boolean'",
        "auth.acme.headers={authorization='SECRET'}",
        "future.key=true",
    ] {
        let output = fixture.run(&[
            "config",
            "batch",
            "--set",
            "model='ollama/new'",
            "--set",
            tail,
        ]);
        assert!(!output.status.success());
        assert!(!error(&output).contains("SECRET"));
        assert_eq!(std::fs::read(fixture.config_path()).unwrap(), before);
    }
}

// Catches: a batch applied to base instead of the selected profile or replacing unrelated keys.
#[test]
fn managed_batch_preserves_target_and_siblings() {
    let fixture = Fixture::new();
    fixture.write_config("# keep\nmodel='ollama/base'\n[profile.ci]\nmodel='ollama/old'\nthinking='high'\nnetwork='off'\n[profile.other]\nmode='plan'\n");
    let output = fixture.run(&[
        "config",
        "batch",
        "--profile",
        "ci",
        "--set",
        "model='ollama/new'",
        "--set",
        "mode='plan'",
        "--unset",
        "network",
    ]);
    assert!(output.status.success(), "{}", error(&output));
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["changed"], true);
    let stored: toml::Value =
        toml::from_str(&std::fs::read_to_string(fixture.config_path()).unwrap()).unwrap();
    assert_eq!(stored["model"].as_str(), Some("ollama/base"));
    assert_eq!(
        stored["profile"]["ci"]["model"].as_str(),
        Some("ollama/new")
    );
    assert_eq!(stored["profile"]["ci"]["thinking"].as_str(), Some("high"));
    assert_eq!(stored["profile"]["ci"]["mode"].as_str(), Some("plan"));
    assert!(stored["profile"]["ci"].get("network").is_none());
    assert_eq!(stored["profile"]["other"]["mode"].as_str(), Some("plan"));
    let listed = fixture.run(&["config", "ls", "--profile", "ci", "--json"]);
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["values"]["model"], "ollama/new");
    assert!(
        std::fs::read_to_string(fixture.config_path())
            .unwrap()
            .starts_with("# keep")
    );
}

// Catches: unbounded, empty, duplicate or mixed-target batches publish.
#[test]
fn managed_batch_rejects_ambiguous_or_unbounded_requests() {
    let fixture = Fixture::new();
    for args in [
        vec!["config", "batch"],
        vec!["config", "batch", "--set", "bare=true", "--unset", "bare"],
        vec![
            "config",
            "batch",
            "--profile",
            "a",
            "--profile",
            "b",
            "--set",
            "bare=true",
        ],
    ] {
        assert!(!fixture.run(&args).status.success());
        assert!(!fixture.config_path().exists());
    }
    let mut args = vec!["config", "batch"];
    for _ in 0..65 {
        args.extend(["--set", "bare=true"]);
    }
    assert!(!fixture.run(&args).status.success());
    assert!(!fixture.config_path().exists());
    let output = fixture.run(&["config", "capabilities"]);
    let capabilities: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(capabilities["atomic_batch"], true);
    assert_eq!(capabilities["max_operations"], 64);
}
