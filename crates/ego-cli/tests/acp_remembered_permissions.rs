//! ACP remembered permissions through the real host and user rule commands.

mod support;

use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text, tool_call};

const PROFILE: &str = "acp";

fn configure(places: &Places) {
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.acp]\nmodel = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"workspace\"\nmode = \"default\"\n",
    )
    .expect("isolated profile configuration");
}

fn stub() -> Stub {
    let call = || Reply::Sse(tool_call("remembered-exec", "exec", r#"{"argv":["ls"]}"#));
    Stub::start(Plan {
        completions: vec![
            call(),
            Reply::Sse(text("first done")),
            call(),
            Reply::Sse(text("second done")),
            call(),
            Reply::Sse(text("third done")),
        ],
        ..Plan::default()
    })
}

fn host(places: &Places, stub: &Stub) -> AcpHost {
    AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(stub))
            .subcommand("acp")
            .args(["--profile", PROFILE])
            .spawn_conversing(),
    )
}

fn prompt(host: &mut AcpHost, places: &Places) -> (String, u64) {
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let request = host.send_request(
        "session/prompt",
        serde_json::json!({
            "sessionId": session,
            "prompt": [{"type": "text", "text": "Run ls"}],
        }),
    );
    (session, request)
}

fn permission(host: &mut AcpHost) -> serde_json::Value {
    loop {
        let frame = host.read_frame();
        if frame["method"] == "session/request_permission" {
            return frame;
        }
    }
}

fn select(host: &mut AcpHost, request: &serde_json::Value, option: &str) {
    host.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": {"outcome": {"outcome": "selected", "optionId": option}},
    }));
}

fn settle_without_question(host: &mut AcpHost, prompt_id: u64) -> serde_json::Value {
    loop {
        let frame = host.read_frame();
        assert_ne!(
            frame["method"], "session/request_permission",
            "a remembered rule did not settle the next session: {frame}"
        );
        if frame["id"].as_u64() == Some(prompt_id) {
            return frame;
        }
    }
}

fn listed_rules(places: &Places, profile: Option<&str>) -> serde_json::Value {
    let ego = Ego::at(places.home_path(), places.workspace_path(), None).subcommand("rules");
    let ego = match profile {
        Some(name) => ego.args(["ls", "--profile", name, "--json"]),
        None => ego.args(["ls", "--json"]),
    };
    let result = ego.run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    serde_json::from_str(&result.stdout).expect("rules ls JSON")
}

#[test]
fn allow_always_writes_one_profile_rule_and_revoke_restores_the_question() {
    let places = Places::new();
    configure(&places);
    let stub = stub();
    let mut first = host(&places, &stub);
    let (_, prompt_id) = prompt(&mut first, &places);
    let question = permission(&mut first);
    let options = question["params"]["options"]
        .as_array()
        .expect("ACP options");
    assert!(options.iter().any(|row| row["optionId"] == "allow-always"));
    assert!(options.iter().any(|row| row["optionId"] == "reject-always"));
    assert!(
        question["params"]["toolCall"]["title"]
            .as_str()
            .is_some_and(|title| title.contains("exec(ls)") && title.contains("cwd")),
        "persistent permission widening was not shown: {question}"
    );
    select(&mut first, &question, "allow-always");
    let first_result = settle_without_question(&mut first, prompt_id);
    assert!(first_result["result"].is_object(), "{first_result}");
    first.finish();

    let profile = listed_rules(&places, Some(PROFILE));
    let rows = profile["rules"].as_array().expect("profile rules");
    assert_eq!(rows.len(), 1, "{profile}");
    assert_eq!(rows[0]["effect"], "allow");
    assert_eq!(rows[0]["rule"], "exec(ls)");
    assert!(
        listed_rules(&places, None)["rules"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let mut second = host(&places, &stub);
    let (_, prompt_id) = prompt(&mut second, &places);
    let second_result = settle_without_question(&mut second, prompt_id);
    assert!(second_result["result"].is_object(), "{second_result}");
    second.finish();

    let rule_id = rows[0]["id"].as_str().expect("rule id");
    let revoked = Ego::at(places.home_path(), places.workspace_path(), None)
        .subcommand("rules")
        .args(["revoke", "--profile", PROFILE, rule_id])
        .run();
    assert_eq!(revoked.code, Some(0), "{}", revoked.stderr);
    assert!(
        listed_rules(&places, Some(PROFILE))["rules"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let mut third = host(&places, &stub);
    let (_, _) = prompt(&mut third, &places);
    let returned = permission(&mut third);
    assert!(returned["params"]["options"].is_array(), "{returned}");
    third.finish();
}

#[test]
fn reject_always_stores_profile_deny_that_beats_a_later_base_allow() {
    let places = Places::new();
    configure(&places);
    let stub = stub();
    let mut first = host(&places, &stub);
    let (_, prompt_id) = prompt(&mut first, &places);
    let question = permission(&mut first);
    select(&mut first, &question, "reject-always");
    let denied = settle_without_question(&mut first, prompt_id);
    assert!(
        denied["result"].is_object() || denied["error"].is_object(),
        "{denied}"
    );
    first.finish();

    let profile = listed_rules(&places, Some(PROFILE));
    let rows = profile["rules"].as_array().expect("profile rules");
    assert_eq!(rows.len(), 1, "{profile}");
    assert_eq!(rows[0]["effect"], "deny");
    assert_eq!(rows[0]["rule"], "exec(ls)");
    let allow = Ego::at(places.home_path(), places.workspace_path(), None)
        .subcommand("rules")
        .args(["allow", "exec(*)"])
        .run();
    assert_eq!(allow.code, Some(0), "{}", allow.stderr);

    let mut second = host(&places, &stub);
    let (_, prompt_id) = prompt(&mut second, &places);
    let denied_again = settle_without_question(&mut second, prompt_id);
    assert!(
        denied_again["result"].is_object() || denied_again["error"].is_object(),
        "{denied_again}"
    );
    second.finish();
}

#[test]
fn concurrent_opposite_rule_makes_allow_always_fail_closed() {
    let places = Places::new();
    configure(&places);
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "conflicted-exec",
                "exec",
                r#"{"argv":["touch","attack-marker"]}"#,
            )),
            Reply::Sse(text("done")),
        ],
        ..Plan::default()
    });
    let mut first = host(&places, &stub);
    let (_, prompt_id) = prompt(&mut first, &places);
    let question = permission(&mut first);
    let title = question["params"]["toolCall"]["title"]
        .as_str()
        .expect("remembered rule preview");
    let rule = title
        .lines()
        .find_map(|line| line.strip_prefix("Remembered rule: "))
        .expect("exact candidate displayed to the host");
    let denied = Ego::at(places.home_path(), places.workspace_path(), None)
        .subcommand("rules")
        .args(["deny", "--profile", PROFILE, rule])
        .run();
    assert_eq!(denied.code, Some(0), "{}", denied.stderr);

    select(&mut first, &question, "allow-always");
    let outcome = settle_without_question(&mut first, prompt_id);
    assert!(
        outcome["result"].is_object() || outcome["error"].is_object(),
        "{outcome}"
    );
    first.finish();
    assert!(
        !places.workspace_path().join("attack-marker").exists(),
        "a conflicting selected allow executed the denied command"
    );
    let profile = listed_rules(&places, Some(PROFILE));
    let rows = profile["rules"].as_array().expect("profile rules");
    assert_eq!(rows.len(), 1, "{profile}");
    assert_eq!(rows[0]["effect"], "deny");
    assert_eq!(rows[0]["rule"], rule);
}
