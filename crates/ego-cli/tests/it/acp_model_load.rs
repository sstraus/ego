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

#[path = "acp_model_load/instance_fixture.rs"]
mod instance_fixture;
use instance_fixture::save_instance;

fn save_local_instance(
    places: &Places,
    stub: &Stub,
    upstream: &str,
    key_env: &str,
    output: u64,
    extra: &str,
) {
    save_instance(
        places.home_path(),
        "openai-compatible",
        &format!("http://{}/v1/", stub.address),
        &format!("vendor = 'openai'\naccess = 'key'\nkey_env = '{key_env}'"),
        upstream,
        output,
        extra,
    );
}

// Catches: a fresh ACP host advertises one configured definition but dispatches or meters another.
#[test]
fn configured_instance_reload_uses_consistent_fresh_host_admission() {
    let places = Places::new();
    let original = Stub::start(Plan::default());
    let replacement = Stub::start(Plan::default());
    let save = |stub: &Stub, upstream: &str, key_env: &str, output: u64, extra: &str| {
        save_local_instance(&places, stub, upstream, key_env, output, extra);
    };
    let launch = |stub: &Stub| {
        let mut host = AcpHost::adopt(
            Ego::at(places.home_path(), places.workspace_path(), Some(stub))
                .subcommand("acp")
                .args(["--profile", "chat"])
                .env("INSTANCE_A_KEY", "fixture-instance-a")
                .env("INSTANCE_B_KEY", "fixture-instance-b")
                .spawn_conversing(),
        );
        host.initialize();
        host
    };

    save(
        &original,
        "deployment-a",
        "INSTANCE_A_KEY",
        2048,
        "old-only",
    );
    let mut first = launch(&original);
    let session = new_session(&mut first, &places);
    select(&mut first, &session, "office/assistant");
    prompt(&mut first, &session);
    save(
        &replacement,
        "deployment-b",
        "INSTANCE_B_KEY",
        4096,
        "new-only",
    );
    // Saving must not refresh an already admitted seat in the first process.
    prompt(&mut first, &session);
    first.finish();

    let mut second = launch(&replacement);
    let loaded = load(&mut second, &places, &session);
    let model = loaded["result"]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == "model")
        .unwrap();
    assert_eq!(model["currentValue"], "office/assistant", "{loaded}");
    let choices = serde_json::to_string(&model["options"]).unwrap();
    assert!(choices.contains("office/new-only"), "{model}");
    assert!(!choices.contains("office/old-only"), "{model}");
    prompt(&mut second, &session);
    second.finish();

    let old_requests = original.generations();
    let new_requests = replacement.generations();
    assert_eq!(old_requests.len(), 2, "{old_requests:?}");
    assert_eq!(new_requests.len(), 1, "{new_requests:?}");
    for request in &old_requests {
        assert_eq!(request["model"], "deployment-a");
    }
    assert_eq!(new_requests[0]["model"], "deployment-b");
    for (stub, expected) in [
        (&original, "Bearer fixture-instance-a"),
        (&replacement, "Bearer fixture-instance-b"),
    ] {
        for request in stub
            .requests()
            .into_iter()
            .filter(|request| request.path.ends_with("/chat/completions"))
        {
            assert_eq!(request.headers.get("authorization").unwrap(), expected);
        }
    }

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
    assert_eq!(admissions.len(), 3, "{admissions:?}");
    for (admission, (wire, output)) in admissions.iter().zip([
        ("deployment-a", 2048),
        ("deployment-a", 2048),
        ("deployment-b", 4096),
    ]) {
        assert_eq!(admission.target().canonical_model().0, "office/assistant");
        assert_eq!(admission.target().wire_model(), wire);
        assert_eq!(admission.capabilities().max_output_tokens, Some(output));
    }
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

#[path = "acp_model_load/catalogue_publication.rs"]
mod catalogue_publication;
