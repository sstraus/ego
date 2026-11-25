//! Packaged host projections and recovery for trusted configured rows.

use super::{load, new_session, prompt, save_instance, save_local_instance, select};
use crate::support::{AcpHost, Ego, Places, Plan, ReleaseGate, Reply, Stub, paced};
use serde_json::{Value, json};

fn listing(places: &Places, key: Option<(&str, &str)>) -> Value {
    let mut command = Ego::at(places.home_path(), places.workspace_path(), None)
        .subcommand("models")
        .args(["--provider", "office", "--json"]);
    if let Some((name, value)) = key {
        command = command.env(name, value);
    }
    let result = command.run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    serde_json::from_str(&result.stdout).expect("catalogue JSON")
}

fn listed_model(listing: &Value) -> &Value {
    listing["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "office/assistant")
        .unwrap_or_else(|| panic!("office/assistant missing: {listing}"))
}

fn mcp_call(mcp: &mut AcpHost, name: &str, arguments: Value) -> Value {
    let response = mcp.request("tools/call", json!({"name": name, "arguments": arguments}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("MCP {name} returned no snapshot: {response}"));
    serde_json::from_str(text).expect("MCP run snapshot")
}

fn settled(mcp: &mut AcpHost, started: &Value) -> Value {
    let run_id = started["run_id"].as_str().expect("admitted run ID");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let status = mcp_call(mcp, "status", json!({"run_id": run_id}));
        if status["status"] != "running" {
            assert_eq!(status["settlement"]["outcome"], "completed", "{status}");
            return status;
        }
        assert!(std::time::Instant::now() < deadline, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

// Catches: the Azure profile admits on one host but loses its configured alias
// or facts on another, or absent named credentials fall back to an ambient key.
#[test]
fn azure_deployment_projects_through_cli_and_acp() {
    let places = Places::new();
    let base = "https://catalogue-fixture.openai.azure.com/openai/v1/";
    save_instance(
        places.home_path(),
        "azure-openai",
        base,
        &format!(
            "vendor = 'azure-openai'\naccess = 'key'\nkey_env = 'OFFICE_AZURE_KEY'\nazure_resource = '{base}'"
        ),
        "azure-deployment-a",
        3072,
        "alternate",
    );

    let absent = listing(&places, Some(("OPENAI_API_KEY", "unrelated-fixture")));
    assert_eq!(
        listed_model(&absent)["availability"]["cause"],
        "missing_credential"
    );
    let available = listing(&places, Some(("OFFICE_AZURE_KEY", "fixture-only")));
    let listed = listed_model(&available);
    assert_eq!(listed["capabilities"]["max_output_tokens"], 3072);
    assert_eq!(listed["capabilities"]["max_context_tokens"], 32000);
    assert_eq!(listed["availability"]["status"], "available");
    assert!(!available.to_string().contains("fixture-only"));

    let mut acp = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .args(["--profile", "chat"])
            .env("OFFICE_AZURE_KEY", "fixture-only")
            .spawn_conversing(),
    );
    acp.initialize();
    let session = new_session(&mut acp, &places);
    select(&mut acp, &session, "office/alternate");
    select(&mut acp, &session, "office/assistant");
    acp.finish();

    let refused = Ego::at(places.home_path(), places.workspace_path(), None)
        .args(["--profile", "chat", "--output", "text", "hello"])
        .run();
    assert_ne!(refused.code, Some(0));
    assert!(
        refused.stderr.contains("OFFICE_AZURE_KEY"),
        "{}",
        refused.stderr
    );
}

// Catches: the headless and public MCP run hosts publish a configured alias
// but dispatch the alias, a different credential, or an unadmitted route.
#[test]
fn configured_alias_reaches_headless_and_mcp_dispatch() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    save_local_instance(
        &places,
        &stub,
        "deployment-shared",
        "OFFICE_KEY",
        2048,
        "other",
    );
    let model = listing(&places, Some(("OFFICE_KEY", "fixture-office")));
    assert_eq!(
        listed_model(&model)["capabilities"]["max_output_tokens"],
        2048
    );
    let command = || {
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .env("OFFICE_KEY", "fixture-office")
    };
    let headless = command()
        .args(["--profile", "chat", "--output", "text", "hello"])
        .run();
    assert_eq!(headless.code, Some(0), "{}", headless.stderr);

    let mut mcp = AcpHost::adopt(
        command()
            .subcommand("mcp-server")
            .args(["--allow-model", "office/assistant"])
            .spawn_conversing(),
    );
    let initialized = mcp.request(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "catalogue-test", "version": "1"}
        }),
    );
    assert_eq!(
        initialized["result"]["protocolVersion"], "2025-11-25",
        "{initialized}"
    );
    mcp.write_frame(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    let started = mcp_call(
        &mut mcp,
        "run",
        json!({
            "request_id": "7b1c1f0e-5a43-4a44-9d1e-3b0a6c7d2e11",
            "root": places.workspace_path(),
            "prompt": "hello",
            "model": "office/assistant",
            "thinking": "off"
        }),
    );
    let first = settled(&mut mcp, &started);
    let replacement = Stub::start(Plan::default());
    save_local_instance(
        &places,
        &replacement,
        "deployment-replacement",
        "NEW_OFFICE_KEY",
        4096,
        "new",
    );
    let resumed = mcp_call(
        &mut mcp,
        "continue",
        json!({
            "request_id": "7b1c1f0e-5a43-4a44-9d1e-3b0a6c7d2e12",
            "run_id": first["run_id"],
            "expected_revision": first["revision"],
            "prompt": "continue"
        }),
    );
    settled(&mut mcp, &resumed);
    mcp.finish();

    assert!(replacement.generations().is_empty());
    let requests = stub.requests();
    let generations: Vec<_> = requests
        .iter()
        .filter(|request| request.path.ends_with("/chat/completions"))
        .collect();
    assert_eq!(generations.len(), 3, "{requests:?}");
    for generation in generations {
        assert_eq!(
            generation.headers.get("authorization").map(String::as_str),
            Some("Bearer fixture-office")
        );
    }
    assert!(
        stub.generations()
            .iter()
            .all(|request| request["model"] == "deployment-shared")
    );
}

// Catches: recovering an active generation dispatches the interrupted request
// against a newly saved instance or leaves the session wedged on its old route.
#[test]
fn interrupted_configured_generation_is_not_redispatched_after_save() {
    let places = Places::new();
    let Reply::Paced { head, tail, .. } = paced("before crash", std::time::Duration::ZERO) else {
        unreachable!("paced returns a split response");
    };
    let gate = ReleaseGate::new();
    let original = Stub::start(Plan {
        completions: vec![Reply::GatedPaced {
            head,
            tail,
            gate: gate.clone(),
        }],
        ..Plan::default()
    });
    let replacement = Stub::start(Plan::default());
    save_local_instance(
        &places,
        &original,
        "deployment-a",
        "OFFICE_KEY",
        2048,
        "old",
    );
    let launch = |stub: &Stub| {
        let mut launched = AcpHost::adopt(
            Ego::at(places.home_path(), places.workspace_path(), Some(stub))
                .subcommand("acp")
                .args(["--profile", "chat"])
                .env("OFFICE_KEY", "fixture-office")
                .spawn_conversing(),
        );
        launched.initialize();
        launched
    };
    let mut first = launch(&original);
    let session = new_session(&mut first, &places);
    first.send_request(
        "session/prompt",
        json!({
            "sessionId": session, "prompt": [{"type": "text", "text": "Interrupted turn"}]
        }),
    );
    loop {
        let frame = first.read_frame();
        if frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
            break;
        }
    }
    save_local_instance(
        &places,
        &replacement,
        "deployment-b",
        "OFFICE_KEY",
        4096,
        "new",
    );
    first.child().kill().expect("kill the owned fixture host");
    gate.release();
    first.finish();

    let mut second = launch(&replacement);
    let loaded = load(&mut second, &places, &session);
    assert!(loaded.to_string().contains("office/new"), "{loaded}");
    assert!(replacement.generations().is_empty());
    prompt(&mut second, &session);
    second.finish();
    let old = original.generations();
    let new = replacement.generations();
    assert_eq!(old.len(), 1, "{old:?}");
    assert_eq!(new.len(), 1, "{new:?}");
    assert_eq!(old[0]["model"], "deployment-a");
    assert_eq!(new[0]["model"], "deployment-b");
}
