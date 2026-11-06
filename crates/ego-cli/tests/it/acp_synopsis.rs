//! Session synopsis observed through the packaged ACP v1 stdio surface.

use crate::support;

use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text};

fn checkpoint() -> &'static str {
    "# Objective\nPreserve the cache diff.\n# Binding constraints and preferences\nKeep tests.\n# Completed work and evidence\nThe cache diff was explained.\n# Work in progress\nReview the changes.\n# Decisions and rationale\nUse the durable checkpoint.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nInspect the tests.\n# Critical references\ncrates/ego-cli/tests/acp_synopsis.rs"
}

fn host(places: &Places, stub: &Stub) -> AcpHost {
    std::fs::write(
        places.home_path().join("config.toml"),
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"workspace\"\n",
    )
    .expect("isolated ACP configuration");
    AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(stub))
            .subcommand("acp")
            .spawn_conversing(),
    )
}

fn prompt(host: &mut AcpHost, session: &str, text: &str) -> Vec<serde_json::Value> {
    let id = host.send_request(
        "session/prompt",
        serde_json::json!({
            "sessionId": session,
            "prompt": [{"type": "text", "text": text}],
        }),
    );
    let mut updates = Vec::new();
    loop {
        let frame = host.read_frame();
        if frame["id"] == id {
            assert!(frame.get("result").is_some(), "prompt failed: {frame}");
            return updates;
        }
        if frame["params"]["update"]["sessionUpdate"] == "session_info_update" {
            updates.push(frame["params"]["update"].clone());
        }
    }
}

fn row<'a>(listed: &'a serde_json::Value, session: &str) -> &'a serde_json::Value {
    listed["result"]["sessions"]
        .as_array()
        .expect("session rows")
        .iter()
        .find(|row| row["sessionId"] == session)
        .unwrap_or_else(|| panic!("session {session} absent: {listed}"))
}

fn manifest(places: &Places, session: &str) -> serde_json::Value {
    let path = places
        .home_path()
        .join("sessions")
        .join(session)
        .join("manifest.json");
    serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|_| panic!("missing {path:?}")))
        .expect("session manifest JSON")
}

#[test]
fn first_idle_publishes_a_durable_title_and_bounded_summary() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text(&format!(
            "The diff adds a cache and its tests. {}",
            "🦀".repeat(200)
        )))],
        ..Plan::default()
    });
    let mut initial = host(&places, &stub);
    initial.initialize();
    let cwd = places.workspace_path().to_str().expect("UTF-8 root");
    let session = initial.new_session(cwd);
    let info = prompt(&mut initial, &session, "Explain the cache diff");
    let latest = info.last().expect("title and summary update after idle");
    assert_eq!(latest["title"], "Explain the cache diff");
    let summary = latest["_meta"]["ego"]["summary"]
        .as_str()
        .expect("ACP summary metadata");
    assert!(summary.contains("cache diff") && summary.contains("adds a cache"));
    assert!(
        summary.len() <= 300,
        "summary exceeds a conservative token bound"
    );

    let listed = initial.request("session/list", serde_json::json!({"cwd": cwd}));
    assert_eq!(row(&listed, &session)["title"], "Explain the cache diff");
    let durable = manifest(&places, &session);
    assert_eq!(durable["title"], "Explain the cache diff");
    assert_eq!(durable["summary"], summary);
    initial.finish();

    std::fs::remove_file(
        places
            .home_path()
            .join("sessions")
            .join(&session)
            .join("manifest.json"),
    )
    .expect("remove rebuildable manifest");
    let mut recovered = host(&places, &stub);
    recovered.initialize();
    let loaded = recovered.request(
        "session/load",
        serde_json::json!({"sessionId": session, "cwd": cwd, "mcpServers": []}),
    );
    assert!(loaded.get("result").is_some(), "load failed: {loaded}");
    let rebuilt = recovered.request("session/list", serde_json::json!({"cwd": cwd}));
    assert_eq!(row(&rebuilt, &session)["title"], "Explain the cache diff");
    assert_eq!(manifest(&places, &session)["summary"], summary);
    recovered.finish();
}

#[test]
fn a_host_that_reconnects_without_the_id_lists_a_session_whose_manifest_is_gone() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("The diff adds a cache and its tests."))],
        ..Plan::default()
    });
    let mut initial = host(&places, &stub);
    initial.initialize();
    let cwd = places.workspace_path().to_str().expect("UTF-8 root");
    let session = initial.new_session(cwd);
    prompt(&mut initial, &session, "Explain the cache diff");
    let summary = manifest(&places, &session)["summary"].clone();
    initial.finish();

    std::fs::remove_file(
        places
            .home_path()
            .join("sessions")
            .join(&session)
            .join("manifest.json"),
    )
    .expect("remove rebuildable manifest");
    let mut reconnected = host(&places, &stub);
    reconnected.initialize();
    let listed = reconnected.request("session/list", serde_json::json!({"cwd": cwd}));
    let listed_row = row(&listed, &session);
    assert_eq!(listed_row["title"], "Explain the cache diff");
    assert_eq!(listed_row["_meta"]["ego"]["summary"], summary);
    reconnected.finish();
}

#[test]
fn compaction_checkpoint_becomes_the_target_synopsis_for_a_reconnecting_host() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("The cache diff was explained.")),
            Reply::Sse(text(checkpoint())),
        ],
        ..Plan::default()
    });
    let cwd = places.workspace_path().to_str().expect("UTF-8 root");
    let mut source = host(&places, &stub);
    source.initialize();
    let session = source.new_session(cwd);
    prompt(&mut source, &session, "Explain the cache diff");
    source.finish();

    let compacted = Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
        .subcommand("compact")
        .args([session.as_str(), "--output", "json"])
        .run();
    assert_eq!(compacted.code, Some(0), "{}", compacted.stderr);
    let report = compacted.report();
    let target = report["target_session_id"]
        .as_str()
        .expect("compaction target");
    let durable = manifest(&places, target);
    let summary = durable["summary"].as_str().expect("target synopsis");
    assert!(summary.contains("Preserve the cache diff"));
    assert!(summary.len() <= 300);

    let mut reconnect = host(&places, &stub);
    reconnect.initialize();
    let listed = reconnect.request("session/list", serde_json::json!({"cwd": cwd}));
    assert_eq!(row(&listed, target)["_meta"]["ego"]["summary"], summary);
    reconnect.finish();
}

#[test]
fn manual_name_survives_a_later_idle() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("First answer.")),
            Reply::Sse(text("Second answer.")),
        ],
        ..Plan::default()
    });
    let cwd = places.workspace_path().to_str().expect("UTF-8 root");
    let mut first = host(&places, &stub);
    first.initialize();
    let session = first.new_session(cwd);
    prompt(&mut first, &session, "First request");
    first.finish();

    let renamed = Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
        .subcommand("resume")
        .args([session.as_str(), "--name", "set=Manual title"])
        .run();
    assert_eq!(renamed.code, Some(0), "{}", renamed.stderr);

    let mut second = host(&places, &stub);
    second.initialize();
    let loaded = second.request(
        "session/load",
        serde_json::json!({"sessionId": session, "cwd": cwd, "mcpServers": []}),
    );
    assert!(loaded.get("result").is_some(), "load failed: {loaded}");
    prompt(&mut second, &session, "Second request");
    let listed = second.request("session/list", serde_json::json!({"cwd": cwd}));
    assert_eq!(row(&listed, &session)["title"], "Manual title");
    assert_eq!(manifest(&places, &session)["title"], "Manual title");
    second.finish();
}

#[test]
fn unusable_answer_records_one_synopsis_failure_without_failing_the_turn() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text(""))],
        ..Plan::default()
    });
    let cwd = places.workspace_path().to_str().expect("UTF-8 root");
    let mut host = host(&places, &stub);
    host.initialize();
    let session = host.new_session(cwd);
    prompt(&mut host, &session, "Explain this file");
    let listed = host.request("session/list", serde_json::json!({"cwd": cwd}));
    assert!(row(&listed, &session)["title"].is_null());
    let durable = manifest(&places, &session);
    assert!(durable["title"].is_null());
    assert_eq!(durable["summary_failures"], 1);
    host.finish();
}
