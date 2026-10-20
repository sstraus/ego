//! Session prompt isolation through the packaged ACP stdio server.
mod support;

use serde_json::{Value, json};
use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text};

fn host<const N: usize>(places: &Places, stub: &Stub, args: [&str; N]) -> AcpHost {
    std::fs::write(
        places.home_path().join("config.toml"),
        "model = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\n[profile.host]\nmodel = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\n",
    )
    .expect("isolated configuration");
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(stub))
            .subcommand("acp")
            .args(["--profile", "host"])
            .args(args)
            .spawn_conversing(),
    );
    host.initialize();
    host
}

fn session(host: &mut AcpHost, places: &Places, ego: Value) -> String {
    let opened = host.request(
        "session/new",
        json!({
            "cwd": places.workspace_path(), "mcpServers": [], "_meta": {"ego": ego},
        }),
    );
    opened["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session refused: {opened}"))
        .to_owned()
}

fn prompt(host: &mut AcpHost, session: &str) {
    let response = host.request(
        "session/prompt",
        json!({
            "sessionId": session, "prompt": [{"type": "text", "text": "Say hello"}],
        }),
    );
    assert!(response["result"].is_object(), "{response}");
}

fn systems(request: &Value) -> Vec<Value> {
    request["messages"]
        .as_array()
        .expect("model messages")
        .iter()
        .filter(|message| message["role"] == "system")
        .cloned()
        .collect()
}

/// Catches ignored session metadata, cross-session leakage, and a changing system prefix.
#[test]
fn per_session_prompt_is_isolated_and_stable_across_turns() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: (0..3).map(|_| Reply::Sse(text("Hello"))).collect(),
        ..Plan::default()
    });
    let mut host = host(&places, &stub, ["--append-system", "launch marker"]);
    let custom = session(
        &mut host,
        &places,
        json!({
            "systemPrompt": {"mode": "append", "text": "session marker"},
        }),
    );
    let ordinary = session(&mut host, &places, json!({}));
    prompt(&mut host, &custom);
    prompt(&mut host, &ordinary);
    prompt(&mut host, &custom);
    host.finish();
    let requests = stub.generations();
    assert_eq!(requests.len(), 3);
    let custom_prefix = systems(&requests[0]);
    assert!(
        serde_json::to_string(&custom_prefix)
            .unwrap()
            .contains("session marker"),
        "per-session prompt never reached model: {custom_prefix:?}"
    );
    assert!(
        serde_json::to_string(&custom_prefix)
            .unwrap()
            .contains("launch marker")
    );
    assert!(
        !serde_json::to_string(&systems(&requests[1]))
            .unwrap()
            .contains("session marker")
    );
    assert_eq!(
        custom_prefix,
        systems(&requests[2]),
        "stable system prefix changed"
    );
}

/// Catches replacement retaining launch text and @file resolving outside the session cwd.
#[test]
fn replace_and_file_append_use_the_session_cwd_and_keep_stable_prefixes() {
    for mode in ["append", "replace"] {
        let places = Places::new();
        places.file("prompt.txt", "custom file marker");
        let stub = Stub::start(Plan {
            completions: (0..2).map(|_| Reply::Sse(text("Hello"))).collect(),
            ..Plan::default()
        });
        let mut host = host(
            &places,
            &stub,
            ["--system", "launch base", "--append-system", "launch tail"],
        );
        let id = session(
            &mut host,
            &places,
            json!({
                "systemPrompt": {"mode": mode, "text": "@prompt.txt"},
            }),
        );
        prompt(&mut host, &id);
        prompt(&mut host, &id);
        host.finish();
        let requests = stub.generations();
        let prefix = systems(&requests[0]);
        let encoded = serde_json::to_string(&prefix).unwrap();
        assert!(encoded.contains("custom file marker"), "{mode}: {encoded}");
        assert_eq!(
            encoded.contains("launch base"),
            mode == "append",
            "{mode}: {encoded}"
        );
        assert_eq!(
            encoded.contains("launch tail"),
            mode == "append",
            "{mode}: {encoded}"
        );
        assert_eq!(prefix, systems(&requests[1]), "{mode}: prefix changed");
    }
}

/// Catches a host bare selection loading ambient instructions after explicit trust.
#[test]
fn session_bare_suppresses_trusted_ambient_instructions() {
    let places = Places::new();
    places.file("AGENTS.md", "ambient instruction marker");
    let stub = Stub::start(Plan {
        completions: (0..2).map(|_| Reply::Sse(text("Hello"))).collect(),
        ..Plan::default()
    });
    let mut host = host(&places, &stub, []);
    let ordinary = session(&mut host, &places, json!({"trust": "trusted"}));
    let bare = session(
        &mut host,
        &places,
        json!({"trust": "trusted", "bare": true}),
    );
    prompt(&mut host, &ordinary);
    prompt(&mut host, &bare);
    host.finish();
    let requests = stub.generations();
    assert!(
        serde_json::to_string(&systems(&requests[0]))
            .unwrap()
            .contains("ambient instruction marker")
    );
    assert!(
        !serde_json::to_string(&systems(&requests[1]))
            .unwrap()
            .contains("ambient instruction marker")
    );
}

/// Catches malformed prompt metadata silently degrading to the launch defaults.
#[test]
fn malformed_prompt_selections_refuse_creation_without_a_model_request() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let mut host = host(&places, &stub, []);
    for ego in [
        json!({"systemPrompt": {"mode": "invalid", "text": "x"}}),
        json!({"systemPrompt": {"mode": "append"}}),
        json!({"systemPrompt": {"mode": "append", "text": 2}}),
        json!({"systemPrompt": {"mode": "append", "text": "x", "unknown": true}}),
        json!({"systemPrompt": null}),
        json!({"bare": "true"}),
        json!({"bare": null}),
    ] {
        let response = host.request(
            "session/new",
            json!({
                "cwd": places.workspace_path(), "mcpServers": [], "_meta": {"ego": ego},
            }),
        );
        assert_eq!(response["error"]["code"], -32602, "{ego}: {response}");
    }
    let listed = host.request("session/list", json!({}));
    assert_eq!(listed["result"]["sessions"], json!([]), "{listed}");
    host.finish();
    assert!(stub.generations().is_empty());
}

/// Catches ACP accepting the bare/instructions conflict that the CLI refuses.
#[test]
fn bare_conflicts_with_explicit_instructions_on_both_surfaces() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    for command in ["run", "acp"] {
        let output = Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand(command)
            .args(["--bare", "--instructions", "AGENTS.md"])
            .run();
        assert_eq!(output.code, Some(2), "{command}: {}", output.stderr);
        assert!(
            output.stderr.contains("--bare") && output.stderr.contains("--instructions"),
            "{}",
            output.stderr
        );
    }
    let mut host = host(&places, &stub, ["--instructions", "AGENTS.md"]);
    let response = host.request(
        "session/new",
        json!({
            "cwd": places.workspace_path(), "mcpServers": [], "_meta": {"ego": {"bare": true}},
        }),
    );
    assert!(response["error"].is_object(), "{response}");
    assert!(
        response.to_string().contains("--bare and --instructions"),
        "{response}"
    );
    host.finish();
    assert!(stub.generations().is_empty());
}

/// Catches launch-wide bare being undone by a per-session false value.
#[test]
fn launch_bare_cannot_be_undone_by_session_false() {
    let places = Places::new();
    places.file("AGENTS.md", "ambient instruction marker");
    places.file("prompt.txt", "launch file marker");
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("Hello"))],
        ..Plan::default()
    });
    let mut host = host(&places, &stub, ["--bare", "--append-system", "@prompt.txt"]);
    let id = session(
        &mut host,
        &places,
        json!({"trust": "trusted", "bare": false}),
    );
    prompt(&mut host, &id);
    host.finish();
    let requests = stub.generations();
    let prefix = serde_json::to_string(&systems(&requests[0])).unwrap();
    assert!(prefix.contains("launch file marker"), "{prefix}");
    assert!(!prefix.contains("ambient instruction marker"), "{prefix}");
}

/// Catches creation prompt selections living only in the first ACP process.
#[test]
fn creation_prompt_selections_survive_restart_before_first_prompt() {
    let places = Places::new();
    places.file("AGENTS.md", "ambient instruction marker");
    let stub = Stub::start(Plan {
        completions: (0..4).map(|_| Reply::Sse(text("Hello"))).collect(),
        ..Plan::default()
    });
    let mut first = host(&places, &stub, ["--append-system", "launch marker"]);
    let options = [
        json!({"trust": "trusted", "systemPrompt": {"mode": "append", "text": "custom append marker"}}),
        json!({"trust": "trusted", "systemPrompt": {"mode": "replace", "text": "custom replace marker"}}),
        json!({"trust": "trusted", "bare": true}),
        json!({"trust": "trusted"}),
    ];
    let ids: Vec<_> = options
        .into_iter()
        .map(|options| session(&mut first, &places, options))
        .collect();
    first.finish();
    assert!(stub.generations().is_empty());
    let mut reopened = host(&places, &stub, ["--append-system", "launch marker"]);
    for id in &ids {
        let loaded = reopened.request(
            "session/load",
            json!({
                "sessionId": id, "cwd": places.workspace_path(), "mcpServers": [],
            }),
        );
        assert!(loaded["result"].is_object(), "{loaded}");
        prompt(&mut reopened, id);
    }
    reopened.finish();
    let requests = stub.generations();
    assert_eq!(requests.len(), 4);
    let prefixes: Vec<_> = requests
        .iter()
        .map(|request| serde_json::to_string(&systems(request)).unwrap())
        .collect();
    assert!(
        prefixes[0].contains("custom append marker"),
        "{}",
        prefixes[0]
    );
    assert!(prefixes[0].contains("launch marker"), "{}", prefixes[0]);
    assert!(
        prefixes[1].contains("custom replace marker"),
        "{}",
        prefixes[1]
    );
    assert!(!prefixes[1].contains("launch marker"), "{}", prefixes[1]);
    assert!(
        !prefixes[2].contains("ambient instruction marker"),
        "{}",
        prefixes[2]
    );
    assert!(
        prefixes[3].contains("ambient instruction marker"),
        "{}",
        prefixes[3]
    );
}
