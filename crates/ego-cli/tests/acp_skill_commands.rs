//! ACP advertised slash commands must admit instructions, not forward command text.
mod support;

use serde_json::json;
use support::{AcpHost, MODEL, Places, Plan, Reply, Stub};

// Catches: ACP advertises a plain skill but headless dispatch sends /name as model text.
#[test]
fn advertised_skill_prompt_runs_skill_instead_of_model_text() {
    let stub = Stub::start(Plan {
        installed: vec![MODEL.to_owned()],
        completions: vec![Reply::Sse(support::text("skill complete"))],
        ..Plan::default()
    });
    let places = Places::new();
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    );
    ego.home_file(
        "skills/capture-meeting/SKILL.md",
        "---\nname: capture-meeting\ndescription: Capture fixture\nargument-hint: '[meeting notes]'\n---\nADMITTED-MEETING-INSTRUCTIONS $ARGUMENTS\n",
    );
    let mut host = AcpHost::adopt(places.ego(&stub).subcommand("acp").spawn_conversing());
    host.initialize();
    let session = host.new_session(ego.workspace_path().to_str().unwrap());
    let catalogue = loop {
        let frame = host.read_frame();
        if frame["params"]["update"]["sessionUpdate"] == "available_commands_update" {
            break frame;
        }
    };
    assert!(
        catalogue["params"]["update"]["availableCommands"]
            .as_array()
            .expect("advertised commands")
            .iter()
            .any(|command| command["name"] == "capture-meeting"
                && command["input"]["hint"] == "[meeting notes]"),
        "{catalogue}"
    );
    let response = host.request(
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":"/capture-meeting notes with spaces"}]}),
    );
    let stderr = host.finish();
    assert_eq!(
        response["result"]["stopReason"], "end_turn",
        "{response} {stderr}"
    );
    let requests = stub.generations();
    assert_eq!(requests.len(), 1);
    let messages = requests[0]["messages"]
        .as_array()
        .expect("provider messages");
    assert!(
        messages.iter().any(|message| message["content"]
            .as_str()
            .is_some_and(|text| text.contains("ADMITTED-MEETING-INSTRUCTIONS notes with spaces"))),
        "advertised skill was not admitted: {}",
        requests[0]
    );
}

// Catches: headless selection uses model visibility, loses raw arguments, or requires a tail.
#[test]
fn advertised_user_only_skill_preserves_empty_quoted_and_multiline_arguments() {
    for arguments in ["", "'two words' $ARGUMENTS", "résumé\nnext line"] {
        let stub = Stub::start(Plan {
            installed: vec![MODEL.to_owned()],
            completions: vec![Reply::Sse(support::text("skill complete"))],
            ..Plan::default()
        });
        let places = Places::new();
        let ego = places.ego(&stub);
        ego.home_file(
            "config.toml",
            "model = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\n",
        );
        ego.home_file(
            "skills/user-check/SKILL.md",
            "---\nname: user-check\ndescription: User fixture\ndisable-model-invocation: true\n---\nUSER-ONLY-INSTRUCTIONS[$ARGUMENTS]\n",
        );
        ego.home_file(
            "skills/model-check/SKILL.md",
            "---\nname: model-check\ndescription: Model fixture\nuser-invocable: false\n---\nMODEL-ONLY-INSTRUCTIONS\n",
        );
        let mut host = AcpHost::adopt(places.ego(&stub).subcommand("acp").spawn_conversing());
        host.initialize();
        let session = host.new_session(ego.workspace_path().to_str().unwrap());
        let catalogue = loop {
            let frame = host.read_frame();
            if frame["params"]["update"]["sessionUpdate"] == "available_commands_update" {
                break frame;
            }
        };
        let names: Vec<_> = catalogue["params"]["update"]["availableCommands"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|command| command["name"].as_str())
            .collect();
        assert!(names.contains(&"user-check"), "{catalogue}");
        assert!(!names.contains(&"model-check"), "{catalogue}");
        let command = if arguments.is_empty() {
            "/user-check".to_owned()
        } else {
            format!("/user-check {arguments}")
        };
        let response = host.request(
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":command}]}),
        );
        let stderr = host.finish();
        assert_eq!(
            response["result"]["stopReason"], "end_turn",
            "{response} {stderr}"
        );
        let requests = stub.generations();
        assert_eq!(requests.len(), 1);
        let expected = format!("USER-ONLY-INSTRUCTIONS[{arguments}]");
        assert!(
            requests[0]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains(&expected))
                }),
            "argument tail {arguments:?} was not admitted: {}",
            requests[0]
        );
    }
}
