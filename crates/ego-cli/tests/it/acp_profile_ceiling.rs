//! Real session/new profile extension and the subsequent control boundary.
use crate::support;
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
                // A terminal failure update must not masquerade as initialized controls.
                assert!(
                    update["params"]["update"]["_meta"]["ego"]["error"].is_null(),
                    "{update}"
                );
                assert!(
                    !update["params"]["update"]["configOptions"]
                        .as_array()
                        .unwrap()
                        .is_empty(),
                    "{update}"
                );
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

fn lifecycle_config(places: &Places) {
    std::fs::write(places.home_path().join("config.toml"),
        "model='ollama/qwen3'\nthinking='off'\n[profile.machine]\nmode='default'\nsandbox='off'\nroots=[]\n[profile.wide]\nmode='yolo'\nsandbox='off'\nroots=[]\n").unwrap();
}

fn lifecycle_host(places: &Places, stub: &Stub) -> AcpHost {
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(stub))
            .subcommand("acp")
            .args(["--profile", "wide", "--sandbox", "off"])
            .spawn_conversing(),
    );
    host.initialize();
    host
}

fn durable_ceiling_session(places: &Places, stub: &Stub) -> String {
    lifecycle_config(places);
    let mut host = lifecycle_host(places, stub);
    let opened = host.request(
        "session/new",
        serde_json::json!({"cwd": places.workspace_path(), "mcpServers": [],
        "_meta": {"ego": {"profile": "wide", "ceilingProfile": "machine", "trust": "trusted"}}}),
    );
    let session = opened["result"]["sessionId"].as_str().unwrap().to_owned();
    let result = host.request(
        "session/prompt",
        serde_json::json!({"sessionId": session, "prompt": [{"type":"text", "text":"Hello"}]}),
    );
    assert!(result["result"].is_object(), "{result}");
    host.finish();
    session
}

fn assert_yolo_refused(host: &mut AcpHost, session: &str) {
    let refused = host.request(
        "session/set_config_option",
        serde_json::json!({"sessionId": session, "configId": "mode", "value": "yolo"}),
    );
    assert!(
        refused["error"].is_object(),
        "reopened ceiling escaped: {refused}"
    );
    // A refusal due to absent/uninitialized controls would give false confidence.
    let allowed = host.request(
        "session/set_config_option",
        serde_json::json!({"sessionId": session, "configId": "mode", "value": "default"}),
    );
    assert!(
        allowed["result"].is_object(),
        "restored controls unavailable: {allowed}"
    );
}

// Catches load/resume seeding the launch profile instead of persisted ceiling authority.
#[test]
fn acp_profile_ceiling_survives_load_and_resume_on_fresh_connections() {
    for method in ["session/load", "session/resume"] {
        let places = Places::new();
        let stub = Stub::start(Plan {
            completions: vec![Reply::Sse(text("source complete"))],
            ..Plan::default()
        });
        let session = durable_ceiling_session(&places, &stub);
        let mut host = lifecycle_host(&places, &stub);
        let opened = host.request(method, serde_json::json!({"sessionId": session, "cwd": places.workspace_path(), "mcpServers": []}));
        assert!(opened["result"].is_object(), "{method}: {opened}");
        assert_yolo_refused(&mut host, &session);
        host.finish();
    }
}

// Catches fork/compact successor policy reverting to an unclamped profile,
// including a successor reopened before it has any run records of its own.
#[test]
fn acp_profile_ceiling_survives_fork_and_compaction_successors() {
    for method in ["session/fork", "_ego/compact"] {
        let places = Places::new();
        let checkpoint = "# Objective\nContinue safely.\n# Binding constraints and preferences\nPreserve profile authority.\n# Completed work and evidence\nSource completed.\n# Work in progress\nReopen successor.\n# Decisions and rationale\nKeep ceiling.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nContinue.\n# Critical references\nSource session.";
        let stub = Stub::start(Plan {
            context_length: Some(32_768),
            completions: vec![
                Reply::Sse(text("source complete")),
                Reply::Sse(text(checkpoint)),
            ],
            ..Plan::default()
        });
        let session = durable_ceiling_session(&places, &stub);
        let mut host = lifecycle_host(&places, &stub);
        let params = if method == "session/fork" {
            serde_json::json!({"sessionId": session, "cwd": places.workspace_path(), "mcpServers": []})
        } else {
            serde_json::json!({"v": 1, "sessionId": session, "requestId": "0198c7f4-9c1f-7a2b-8c3d-4e5f60718294"})
        };
        let forked = host.request(method, params);
        let field = if method == "session/fork" {
            "sessionId"
        } else {
            "targetSessionId"
        };
        let child = forked["result"][field]
            .as_str()
            .unwrap_or_else(|| panic!("{method}: {forked}"))
            .to_owned();
        if method == "session/fork" {
            assert_yolo_refused(&mut host, &child);
        }
        host.finish();
        let mut host = lifecycle_host(&places, &stub);
        let opened = host.request("session/load", serde_json::json!({"sessionId": child, "cwd": places.workspace_path(), "mcpServers": []}));
        assert!(opened["result"].is_object(), "{method} successor: {opened}");
        assert_yolo_refused(&mut host, &child);
        host.finish();
    }
}

// Catches a missing persisted ceiling silently becoming an absent ceiling on reopen.
#[test]
fn acp_profile_ceiling_missing_on_reopen_refuses_instead_of_widening() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("source complete"))],
        ..Plan::default()
    });
    let session = durable_ceiling_session(&places, &stub);
    std::fs::write(places.home_path().join("config.toml"), "model='ollama/qwen3'\nthinking='off'\n[profile.wide]\nmode='yolo'\nsandbox='off'\nroots=[]\n").unwrap();
    let mut host = lifecycle_host(&places, &stub);
    for method in [
        "session/load",
        "session/resume",
        "session/fork",
        "_ego/compact",
    ] {
        let params = if method == "_ego/compact" {
            serde_json::json!({"v":1,"sessionId":session,"requestId":"0198c7f4-9c1f-7a2b-8c3d-4e5f60718294"})
        } else {
            serde_json::json!({"sessionId":session,"cwd":places.workspace_path(),"mcpServers":[]})
        };
        let refused = host.request(method, params);
        assert!(refused["error"].is_object(), "{method}: {refused}");
        assert!(
            refused["error"].to_string().contains("machine"),
            "unrelated refusal: {refused}"
        );
    }
    host.finish();
}

// Catches: session/new reports strict but the real prompt uses the wider Adaptive launch ceiling.
#[cfg(target_os = "macos")]
#[test]
fn adaptive_launch_preserves_strict_profile_through_acp_prompt() {
    use ego_core::evidence::{EvidenceLogSource, EvidenceRecord};
    use ego_tools::store::{JsonlStore, OsIo};
    for sandbox in ["ro", "workspace"] {
        let places = Places::new();
        std::fs::write(places.home_path().join("config.toml"),
            format!("model='ollama/qwen3'\nthinking='off'\n[profile.strict]\nsandbox='{sandbox}'\nroots=[]")).unwrap();
        let stub = Stub::start(Plan {
            completions: vec![Reply::Sse(text("done"))],
            ..Plan::default()
        });
        let mut host = AcpHost::adopt(
            Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
                .subcommand("acp")
                .spawn_conversing(),
        );
        host.initialize();
        let opened = host.request(
            "session/new",
            serde_json::json!({"cwd": places.workspace_path(), "mcpServers": [],
            "_meta": {"ego": {"profile": "strict", "trust": "trusted"}}}),
        );
        assert_eq!(
            opened["result"]["_meta"]["ego"]["effective"]["sandbox"], sandbox,
            "{opened}"
        );
        let session = opened["result"]["sessionId"].as_str().unwrap();
        let reply = host.request(
            "session/prompt",
            serde_json::json!({"sessionId": session,
            "prompt": [{"type": "text", "text": "Hello"}]}),
        );
        assert!(reply["result"].is_object(), "{reply}");
        let records = JsonlStore::new(
            places.home_path().join("sessions"),
            places.workspace_path(),
            Box::new(OsIo),
        )
        .evidence_records(&session.parse().unwrap())
        .unwrap();
        let level = records
            .into_iter()
            .find_map(|record| match record {
                EvidenceRecord::RunContext(context) => Some(context.execution().confinement()),
                _ => None,
            })
            .unwrap();
        assert_eq!(level.as_str(), sandbox);
        host.finish();
    }
}
