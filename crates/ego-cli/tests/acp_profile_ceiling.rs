//! Real session/new profile extension and the subsequent control boundary.
mod support;
use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text};

fn host(places: &Places) -> AcpHost {
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .args(["--sandbox", "off"])
            .spawn_conversing(),
    );
    host.initialize();
    host
}

// Catches widening accepted silently, narrower values overwritten, and default roots restored after a drop.
#[test]
fn acp_profile_ceiling_clamps_with_warnings_and_keeps_narrower_roots() {
    let places = Places::new();
    let allowed_root = tempfile::tempdir().unwrap();
    let allowed = allowed_root.path().join("allowed");
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(&allowed).unwrap();
    std::fs::write(
        places.home_path().join("config.toml"),
        format!(
            r#"
[profile.machine]
mode = "default"
sandbox = "workspace"
roots = [{{path = "{}", access = "read-write"}}]
[profile.wide]
mode = "yolo"
sandbox = "off"
roots = [{{path = "{}", access = "read-write"}}]
[profile.narrow]
mode = "plan"
sandbox = "ro"
roots = [{{path = "{}", access = "read"}}]
"#,
            allowed.display(),
            outside.path().display(),
            allowed.display()
        ),
    )
    .unwrap();
    let mut host = host(&places);
    for (profile, mode, sandbox, warning_count, roots) in [
        ("wide", "default", "workspace", 3, 0),
        ("narrow", "plan", "ro", 0, 1),
    ] {
        let opened = host.request(
            "session/new",
            serde_json::json!({"cwd": places.workspace_path(), "mcpServers": [],
            "_meta": {"ego": {"profile": profile, "ceilingProfile": "machine"}}}),
        );
        assert!(opened["result"]["sessionId"].is_string(), "{opened}");
        let meta = &opened["result"]["_meta"]["ego"];
        assert_eq!(meta["effective"]["mode"], mode, "{opened}");
        assert_eq!(meta["effective"]["sandbox"], sandbox, "{opened}");
        assert_eq!(
            meta["effective"]["roots"].as_array().unwrap().len(),
            roots,
            "{opened}"
        );
        assert_eq!(
            meta["warnings"].as_array().unwrap().len(),
            warning_count,
            "{opened}"
        );
        // Deferred measurement publishes the picker after the creation response.
        loop {
            let update = host.read_frame();
            if update["params"]["update"]["sessionUpdate"] == "config_option_update" {
                break;
            }
        }
        for (config_id, value) in [("mode", "yolo"), ("sandbox", "off")] {
            let refused = host.request(
                "session/set_config_option",
                serde_json::json!({
                "sessionId": opened["result"]["sessionId"], "configId": config_id, "value": value}),
            );
            assert!(refused["error"].is_object(), "ceiling escaped: {refused}");
        }
    }
    host.finish();
}

// Catches a truthful creation response whose subsequent prompt runs the wider launch profile.
#[test]
fn acp_profile_ceiling_reaches_real_prompt_preparation() {
    use ego_core::evidence::{EvidenceLogSource, EvidenceRecord};
    use ego_tools::store::{JsonlStore, OsIo};
    let places = Places::new();
    std::fs::write(places.home_path().join("config.toml"), "model='ollama/qwen3'\nthinking='off'\n[profile.machine]\nmode='default'\nsandbox='workspace'\nroots=[]\n[profile.wide]\nmode='yolo'\nsandbox='off'\nroots=[]\n").unwrap();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("done"))],
        ..Plan::default()
    });
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand("acp")
            .args(["--profile", "wide", "--sandbox", "off"])
            .spawn_conversing(),
    );
    host.initialize();
    let opened = host.request(
        "session/new",
        serde_json::json!({"cwd": places.workspace_path(), "mcpServers": [],
        "_meta": {"ego": {"profile": "wide", "ceilingProfile": "machine", "trust": "trusted"}}}),
    );
    let session = opened["result"]["sessionId"].as_str().unwrap();
    let result = host.request(
        "session/prompt",
        serde_json::json!({"sessionId": session, "prompt": [{"type":"text", "text":"Hello"}]}),
    );
    assert!(result["result"].is_object(), "{result}");
    let evidence = JsonlStore::new(
        places.home_path().join("sessions"),
        places.workspace_path(),
        Box::new(OsIo),
    )
    .evidence_records(&session.parse().unwrap())
    .unwrap();
    let confinement = evidence
        .into_iter()
        .find_map(|record| match record {
            EvidenceRecord::RunContext(context) => Some(context.execution().confinement()),
            _ => None,
        })
        .unwrap();
    assert_eq!(confinement.as_str(), "workspace");
    assert_eq!(stub.generations().len(), 1);
    host.finish();
}

// Catches a malformed or missing ceiling treated as absent, and no-ceiling behavior changing.
#[test]
fn acp_profile_ceiling_rejects_bad_names_and_preserves_absent_ceiling() {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.wide]\nmode='yolo'\nsandbox='off'\nroots=[]\n",
    )
    .unwrap();
    let mut host = host(&places);
    for ceiling in [serde_json::json!(false), serde_json::json!("missing")] {
        let refused = host.request(
            "session/new",
            serde_json::json!({"cwd": places.workspace_path(), "mcpServers": [],
            "_meta": {"ego": {"profile": "wide", "ceilingProfile": ceiling}}}),
        );
        assert!(refused["error"].is_object(), "{refused}");
    }
    let opened = host.request(
        "session/new",
        serde_json::json!({"cwd": places.workspace_path(), "mcpServers": [],
        "_meta": {"ego": {"profile": "wide"}}}),
    );
    assert_eq!(
        opened["result"]["_meta"]["ego"]["effective"]["mode"], "yolo",
        "{opened}"
    );
    assert_eq!(
        opened["result"]["_meta"]["ego"]["effective"]["sandbox"], "off",
        "{opened}"
    );
    assert_eq!(
        opened["result"]["_meta"]["ego"]["warnings"],
        serde_json::json!([])
    );
    host.finish();
}
