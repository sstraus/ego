//! A session's explicit model choice survives a new ACP connection.
use crate::support;

use support::{AcpHost, Ego, Places, Plan, Stub};

fn host(places: &Places, stub: &Stub) -> AcpHost {
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(stub))
            .subcommand("acp")
            .args(["--profile", "chat"])
            .spawn_conversing(),
    );
    host.initialize();
    host
}

fn setup() -> (Places, Stub) {
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.chat]\nmodel = 'ollama/gemma4:e4b-mlx'\nthinking = 'off'\nsandbox = 'off'\n",
    )
    .expect("profile config");
    let stub = Stub::start(Plan {
        installed: vec!["qwen3".into(), "gemma4:e4b-mlx".into()],
        ..Plan::default()
    });
    (places, stub)
}

fn new_session(host: &mut AcpHost, places: &Places) -> String {
    let session = host.new_session(places.workspace_path().to_str().unwrap());
    loop {
        let frame = host.read_frame();
        if frame["params"]["sessionId"] == session
            && frame["params"]["update"]["sessionUpdate"] == "config_option_update"
        {
            return session;
        }
    }
}

fn select(host: &mut AcpHost, session: &str, model: &str) {
    let answer = host.request(
        "session/set_config_option",
        serde_json::json!({"sessionId": session, "configId": "model", "value": model}),
    );
    assert!(answer["result"].is_object(), "{answer}");
}

fn prompt(host: &mut AcpHost, session: &str) {
    let answer = host.request(
        "session/prompt",
        serde_json::json!({"sessionId": session, "prompt": [{"type": "text", "text": "Say hello"}]}),
    );
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
}

fn load(host: &mut AcpHost, places: &Places, session: &str) -> serde_json::Value {
    let answer = host.request(
        "session/load",
        serde_json::json!({"sessionId": session, "cwd": places.workspace_path(), "mcpServers": []}),
    );
    assert!(answer["result"].is_object(), "{answer}");
    answer
}

// Catches: session/load silently replacing a selected model with the profile default.
#[test]
fn resumed_session_sends_and_meters_the_selected_model() {
    let (places, stub) = setup();
    let mut first = host(&places, &stub);
    let session = new_session(&mut first, &places);
    select(&mut first, &session, "ollama/qwen3");
    prompt(&mut first, &session);
    first.finish();

    let mut second = host(&places, &stub);
    let loaded = load(&mut second, &places, &session);
    prompt(&mut second, &session);
    second.finish();
    let requests = stub.generations();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(requests[0]["model"], "qwen3");
    assert_eq!(
        requests[1]["model"], "qwen3",
        "resume changed the provider model"
    );
    let model = loaded["result"]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == "model")
        .unwrap();
    assert_eq!(model["currentValue"], "ollama/qwen3");
    use ego_core::evidence::EvidenceRecord;
    use ego_core::store::session_store::{SessionEntry, SessionStore};
    use futures::StreamExt;
    let store = ego_tools::store::JsonlStore::new(
        places.home_path().join("sessions"),
        places.workspace_path(),
        Box::new(ego_tools::store::OsIo),
    );
    let entries = futures::executor::block_on(
        store
            .read(&session.parse().unwrap())
            .unwrap()
            .collect::<Vec<_>>(),
    );
    let admissions: Vec<_> = entries
        .into_iter()
        .filter_map(|entry| match entry.unwrap() {
            SessionEntry::Evidence(EvidenceRecord::GenerationAdmission(admission)) => {
                Some(admission)
            }
            _ => None,
        })
        .collect();
    assert_eq!(admissions.len(), 2, "{admissions:?}");
    assert_eq!(admissions[1].target().canonical_model().0, "ollama/qwen3");
}

// Catches: loading a selected session that has not yet generated loses the choice.
#[test]
fn selected_model_survives_load_before_the_first_prompt() {
    let (places, stub) = setup();
    let mut first = host(&places, &stub);
    let session = new_session(&mut first, &places);
    select(&mut first, &session, "ollama/qwen3");
    first.finish();
    let mut second = host(&places, &stub);
    load(&mut second, &places, &session);
    prompt(&mut second, &session);
    second.finish();
    assert_eq!(stub.generations()[0]["model"], "qwen3");
}

// Catches: restoring a previously used default as though it were an explicit selection.
#[test]
fn an_unselected_session_uses_the_profile_default_after_load() {
    let (places, stub) = setup();
    let mut first = host(&places, &stub);
    let session = new_session(&mut first, &places);
    first.finish();
    let mut second = host(&places, &stub);
    load(&mut second, &places, &session);
    prompt(&mut second, &session);
    second.finish();
    assert_eq!(stub.generations()[0]["model"], "gemma4:e4b-mlx");
}

// Catches: saving only the initial pick, then losing a later model switch on load.
#[test]
fn the_latest_model_choice_wins_after_a_completed_turn() {
    let (places, stub) = setup();
    let mut first = host(&places, &stub);
    let session = new_session(&mut first, &places);
    select(&mut first, &session, "ollama/gemma4:e4b-mlx");
    prompt(&mut first, &session);
    select(&mut first, &session, "ollama/qwen3");
    first.finish();
    let mut second = host(&places, &stub);
    load(&mut second, &places, &session);
    prompt(&mut second, &session);
    second.finish();
    let requests = stub.generations();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], "gemma4:e4b-mlx");
    assert_eq!(requests[1]["model"], "qwen3");
}
