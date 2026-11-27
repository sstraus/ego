//! Settlement seen by a real ACP v1 stdio host of the packaged binary.

use crate::support;

use base64::Engine as _;
use ego_cli::auth::{AuthEntry, AuthStore};
use support::{AcpHost, Ego, Places, Plan, ReleaseGate, Reply, Stub, paced, text, tool_call};

const SETTLEMENT_SANDBOX: &str = support::PLATFORM_SANDBOX;

const HOLD_REQUEST: &str = "00000000-0000-0000-0000-000000000154";

/// Catches: unavailable elicitation poisons a session as uncertain, or loses
/// the durable question instead of allowing an explicit validated answer.
#[test]
fn unavailable_question_is_known_and_explicit_answer_resumes_original_turn() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "pending-question",
                "ask_question",
                r#"{"prompt":"Choose a continuation","mode":"single","options":[{"id":"continue","label":"Continue"}],"free_text":false}"#,
            )),
            Reply::Sse(text("continued after the answer")),
        ],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    host.initialize(); // No form capability: the real adapter leaves input pending.
    let session = host.new_session(places.workspace_path().to_str().unwrap());
    let first = host.request("session/prompt", prompt(&session));
    let message = first["error"]["message"]
        .as_str()
        .expect("pending input diagnostic");
    assert!(message.contains("still requires an answer"), "{first}");
    assert!(!message.contains("uncertain"), "{first}");
    let question = message
        .split("question ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    let requests_before = stub.requests().len();
    let mut repeated = prompt(&session);
    repeated["prompt"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "type": "image",
            "mimeType": "image/png",
            "data": base64::engine::general_purpose::STANDARD.encode(support::PNG_1X1),
        }));
    let next = host.request("session/prompt", repeated);
    let next_message = next["error"]["message"]
        .as_str()
        .expect("input remains pending");
    assert!(!next_message.contains("uncertain"), "{next}");
    assert!(next_message.contains(question), "{next}");
    assert_eq!(
        stub.requests().len(),
        requests_before,
        "pending input must not resolve the provider or acquire attachments"
    );
    let image_blob = places
        .home_path()
        .join("sessions")
        .join(&session)
        .join("evidence")
        .join(format!(
            "{}.blob",
            ego_core::digest::sha256_hex(support::PNG_1X1)
        ));
    assert!(
        !image_blob.exists(),
        "refused prompt left an orphaned image blob"
    );
    assert_eq!(
        stub.generations().len(),
        1,
        "pending input must not generate again"
    );
    host.finish();

    let invalid = Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
        .platform_sandbox()
        .subcommand("resume")
        .args([
            &session,
            "--answer",
            &format!("{question}=\"invalid-choice\""),
            "--output",
            "json",
        ])
        .run();
    assert_eq!(invalid.code, Some(2), "{}", invalid.stderr);
    assert_eq!(
        stub.generations().len(),
        1,
        "invalid input must not consume the question"
    );

    let answered = Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
        .platform_sandbox()
        .subcommand("resume")
        .args([
            &session,
            "--answer",
            &format!("{question}=\"continue\""),
            "--output",
            "json",
        ])
        .run();
    assert_eq!(answered.code, Some(0), "{}", answered.stderr);
    assert!(
        answered.stdout.contains("continued after the answer"),
        "{}",
        answered.stdout
    );
    assert_eq!(stub.generations().len(), 2, "resume continues exactly once");
    let generations = stub.generations();
    let generation = generations.last().unwrap();
    let messages = generation["messages"].as_array().unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "user" && message["content"] == "Run ls")
            .count(),
        1,
        "the original user prompt must not be submitted twice"
    );
    // Runtime context also uses the user role. Assert adjacency to the original
    // tool call instead of assuming every user-role message is a new prompt.
    assert!(
        messages.windows(2).any(|pair| {
            pair[0]["role"] == "assistant"
                && pair[0]["tool_calls"][0]["id"] == "pending-question"
                && pair[1]["role"] == "tool"
                && pair[1]["tool_call_id"] == "pending-question"
                && pair[1]["content"] == "continue"
        }),
        "the answer must immediately complete the original question tool call"
    );
}

fn host(places: &Places, stub: &Stub) -> AcpHost {
    std::fs::write(
        places.home_path().join("config.toml"),
        format!(
            "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"{SETTLEMENT_SANDBOX}\"\n"
        ),
    )
    .expect("isolated ACP configuration");
    AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(stub))
            .subcommand("acp")
            .spawn_conversing(),
    )
}

fn prompt(session: &str) -> serde_json::Value {
    serde_json::json!({
        "sessionId": session,
        "prompt": [{"type": "text", "text": "Run ls"}],
    })
}

#[test]
fn retired_configured_model_is_a_single_actionable_pre_send_error() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    AuthStore::new(places.home_path())
        .put(
            "connection:fixture",
            AuthEntry::OAuth {
                access: "head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig".into(),
                refresh: None,
                expires: None,
                scope_binding: None,
            },
        )
        .expect("isolated test credential");
    std::fs::write(
        places.home_path().join("config.toml"),
        format!("model = \"openai-codex/gpt-5.6-sol\"\nsandbox = \"{SETTLEMENT_SANDBOX}\"\n\n[connections.fixture]\nname=\"Fixture\"\nvendor=\"openai\"\naccess=\"oauth\"\n"),
    )
    .expect("isolated ACP configuration");
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand("acp")
            .spawn_conversing(),
    );
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let answer = host.request("session/prompt", prompt(&session));
    let message = answer["error"]["message"]
        .as_str()
        .expect("ACP error message");
    assert!(message.contains("configuration error"), "{answer}");
    assert!(message.contains("retired"), "{answer}");
    assert!(message.contains("openai-codex/gpt-6-sol"), "{answer}");
    assert!(!message.contains("uncertain"), "{answer}");
    let detail = answer["error"]["data"].as_str().unwrap_or("");
    assert_eq!(
        format!("{message} {detail}")
            .matches("openai-codex/gpt-5.6-sol")
            .count(),
        1,
        "retirement detail repeated: {answer}"
    );
    assert!(
        stub.requests()
            .iter()
            .all(|request| request.path == "/api/tags"),
        "model refusal reached generation: {:?}",
        stub.requests()
    );
    host.finish();
}

#[test]
fn unknown_configured_model_is_a_configuration_error_without_recovery() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    std::fs::write(
        places.home_path().join("config.toml"),
        format!("model = \"openai/no-such-model\"\nsandbox = \"{SETTLEMENT_SANDBOX}\"\n"),
    )
    .expect("isolated ACP configuration");
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .env("OPENAI_API_KEY", "sk-test")
            .subcommand("acp")
            .spawn_conversing(),
    );
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let answer = host.request("session/prompt", prompt(&session));
    let message = answer["error"]["message"]
        .as_str()
        .expect("ACP error message");
    assert!(message.contains("configuration error"), "{answer}");
    assert!(
        message.contains("no capabilities are configured"),
        "{answer}"
    );
    assert!(!message.contains("uncertain"), "{answer}");
    assert!(answer["error"].get("data").is_none(), "{answer}");
    assert!(
        stub.requests()
            .iter()
            .all(|request| request.path == "/api/tags"),
        "unknown model reached generation: {:?}",
        stub.requests()
    );
    host.finish();
}

#[test]
fn cancelling_an_open_permission_settles_the_packaged_prompt_as_cancelled() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(tool_call(
            "permission-call",
            "exec",
            r#"{"argv":["touch","permission-fixture"]}"#,
        ))],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let prompt_id = host.send_request("session/prompt", prompt(&session));
    let permission = loop {
        let frame = host.read_frame();
        if frame["method"] == "session/request_permission" {
            break frame;
        }
        assert_ne!(
            frame["id"], prompt_id,
            "prompt completed before requesting permission: {frame}"
        );
    };
    host.write_frame(serde_json::json!({
        "jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session},
    }));
    host.write_frame(serde_json::json!({
        "jsonrpc": "2.0", "id": permission["id"],
        "result": {"outcome": {"outcome": "cancelled"}},
    }));
    let answer = loop {
        let frame = host.read_frame();
        if frame["id"] == prompt_id {
            break frame;
        }
    };
    assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
    host.finish();
}

#[test]
fn a_reconnecting_stdio_host_discovers_the_durable_hold_and_its_error_code() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let cwd = places.workspace_path().to_str().expect("UTF-8 root");
    let mut first = host(&places, &stub);
    first.initialize();
    let session = first.new_session(cwd);
    let paused = first.request(
        "_ego/pause",
        serde_json::json!({
            "v": 1, "sessionId": session, "requestId": HOLD_REQUEST,
        }),
    );
    assert_eq!(paused["result"]["state"], "paused", "{paused}");
    first.finish();

    let mut second = host(&places, &stub);
    second.initialize();
    let listed = second.request("session/list", serde_json::json!({"cwd": cwd}));
    let row = listed["result"]["sessions"]
        .as_array()
        .expect("session rows")
        .iter()
        .find(|row| row["sessionId"] == session)
        .expect("held row");
    assert_eq!(row["_meta"]["ego"]["hold"]["state"], "paused", "{listed}");
    let loaded = second.request(
        "session/load",
        serde_json::json!({
            "sessionId": session, "cwd": cwd, "mcpServers": [],
        }),
    );
    assert_eq!(
        loaded["result"]["_meta"]["ego"]["hold"]["state"], "paused",
        "{loaded}"
    );
    let refused = second.request("session/prompt", prompt(&session));
    assert_eq!(refused["error"]["code"], -32011, "{refused}");
    second.finish();
}

#[test]
fn pausing_a_streaming_stdio_turn_settles_with_the_held_code() {
    let places = Places::new();
    let Reply::Paced { head, tail, .. } = paced("before pause", std::time::Duration::ZERO) else {
        unreachable!("paced produces a split reply");
    };
    let gate = ReleaseGate::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::GatedPaced {
            head,
            tail,
            gate: gate.clone(),
        }],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let prompt_id = host.send_request("session/prompt", prompt(&session));
    loop {
        let frame = host.read_frame();
        if frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
            break;
        }
        assert_ne!(
            frame["id"], prompt_id,
            "prompt completed before streaming began: {frame}"
        );
    }
    let pause_id = host.send_request(
        "_ego/pause",
        serde_json::json!({
            "v": 1, "sessionId": session, "requestId": HOLD_REQUEST,
        }),
    );
    let pending = loop {
        let frame = host.read_frame();
        if frame["id"] == pause_id {
            break frame;
        }
    };
    assert_eq!(pending["result"]["state"], "pending", "{pending}");
    gate.release();
    let answered = loop {
        let frame = host.read_frame();
        if frame["id"] == prompt_id {
            break frame;
        }
    };
    assert_eq!(answered["error"]["code"], -32011, "{answered}");
    let listed = host.request(
        "session/list",
        serde_json::json!({
            "cwd": places.workspace_path().to_str().expect("UTF-8 root"),
        }),
    );
    let row = listed["result"]["sessions"]
        .as_array()
        .expect("session rows")
        .iter()
        .find(|row| row["sessionId"] == session)
        .expect("paused row");
    assert_eq!(row["_meta"]["ego"]["hold"]["state"], "paused", "{listed}");
    host.finish();
}

/// Story 094-c90d: the live host run cancelled a streaming turn and its next
/// prompt failed. Catches: a cancelled streaming generation leaving the
/// session unable to run, or the next request carrying the half-finished turn
/// in a shape the provider rejects.
#[test]
fn cancelling_a_streaming_stdio_turn_leaves_the_session_able_to_run_the_next_prompt() {
    let places = Places::new();
    let Reply::Paced { head, tail, .. } = paced("before cancel", std::time::Duration::ZERO) else {
        unreachable!("paced produces a split reply");
    };
    let gate = ReleaseGate::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::GatedPaced {
                head,
                tail,
                gate: gate.clone(),
            },
            Reply::Sse(text("recovered")),
        ],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let prompt_id = host.send_request("session/prompt", prompt(&session));
    loop {
        let frame = host.read_frame();
        if frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
            break;
        }
        assert_ne!(
            frame["id"], prompt_id,
            "prompt completed before streaming began: {frame}"
        );
    }
    host.write_frame(serde_json::json!({
        "jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session},
    }));
    // The notification has no reply to wait for; give the agent time to
    // observe it while the generation is still held open.
    std::thread::sleep(std::time::Duration::from_millis(500));
    gate.release();
    let cancelled = loop {
        let frame = host.read_frame();
        if frame["id"] == prompt_id {
            break frame;
        }
    };
    assert_eq!(
        cancelled["result"]["stopReason"], "cancelled",
        "{cancelled}"
    );

    let next_id = host.send_request("session/prompt", prompt(&session));
    let next = loop {
        let frame = host.read_frame();
        if frame["id"] == next_id {
            break frame;
        }
    };
    assert_eq!(next["result"]["stopReason"], "end_turn", "{next}");
    assert_eq!(
        stub.generations().len(),
        2,
        "the second prompt reached the provider"
    );
    host.finish();
}

/// Story 415-0490 — catches: an ACP seat stops after six failed attempts,
/// or its force action is unavailable while a prompt response is outstanding.
#[test]
fn outage_415_acp_recovers_without_a_new_prompt_and_force_retry_is_routable() {
    let places = Places::new();
    let mut completions = vec![
        Reply::StatusRetryAfter {
            status: 503,
            seconds: 1
        };
        8
    ];
    completions.push(Reply::Sse(text("recovered")));
    let stub = Stub::start(Plan {
        completions,
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let prompt_id = host.send_request("session/prompt", prompt(&session));
    let mut attempts = Vec::new();
    let mut forced = Vec::new();
    let mut accepted = 0;
    let mut cleared = false;
    let answer = loop {
        let frame = host.read_frame();
        if frame["id"] == prompt_id {
            break frame;
        }
        if forced.iter().any(|id| frame["id"] == *id) {
            assert_eq!(frame["result"]["state"], "accepted", "{frame}");
            accepted += 1;
            continue;
        }
        let update = &frame["params"]["update"];
        if let Some(status) = update["_meta"]["ego"].get("providerRetry") {
            if status.is_null() {
                cleared = true;
                continue;
            }
            assert_eq!(status["phase"], "recovery");
            // A 503 is a capacity refusal: the 1-second Retry-After is shorter
            // than each capacity step, so the step wins (#855-9071).
            let step = [60_000, 120_000].get(attempts.len()).copied();
            assert_eq!(status["delayMs"], step.unwrap_or(300_000));
            assert_eq!(status["capacity"], true);
            assert!(status["recoveryRemainingMs"].as_u64().unwrap() > 0);
            assert_eq!(status["force"]["method"], "_ego/retry");
            assert!(status["text"].as_str().unwrap().contains("retry now"));
            attempts.push(status["attempt"].as_u64().unwrap());
            forced.push(
                host.send_request("_ego/retry", serde_json::json!({"v":1,"sessionId":session})),
            );
        }
    };
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert_eq!(attempts, (1..=8).collect::<Vec<_>>());
    assert_eq!(accepted, 8);
    assert!(cleared, "retry status must clear before recovered output");
    assert_eq!(stub.generations().len(), 9);
    host.finish();
}

/// Catches: a received HTTP 400 becoming Uncertain and blocking the next ACP
/// prompt, or settlement rewriting the failed attempt's delivery evidence.
#[test]
fn known_http_400_does_not_wedge_the_next_acp_prompt_or_erase_delivery() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400), Reply::Sse(text("recovered"))],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    let failed = host.request("session/prompt", prompt(&session));
    let message = failed["error"]["message"].as_str().expect("failed turn");
    assert_eq!(
        message, "the turn failed: the provider returned HTTP 400",
        "{failed}"
    );
    assert!(failed["error"].get("data").is_none(), "{failed}");
    assert_eq!(
        stub.generations().len(),
        1,
        "a usage refusal is not retried"
    );

    let log = std::fs::read_to_string(
        places
            .home_path()
            .join("sessions")
            .join(&session)
            .join("events.jsonl"),
    )
    .expect("durable failed turn journal");
    let attempt = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("journal record"))
        .filter_map(|record| record.get("payload").cloned())
        .filter_map(|payload| {
            serde_json::from_value::<ego_core::store::session_store::SessionEntry>(payload).ok()
        })
        .find_map(|entry| match entry {
            ego_core::store::session_store::SessionEntry::Event(event) => match event.kind {
                ego_core::event::domain::EventKind::AttemptSettled { outcome, .. } => Some(outcome),
                _ => None,
            },
            _ => None,
        })
        .expect("failed attempt evidence");
    assert_eq!(
        attempt.delivery(),
        ego_core::generator::attempt::DeliveryCertainty::PossiblySent,
        "settlement must preserve delivery evidence"
    );

    let next = host.request("session/prompt", prompt(&session));
    assert_eq!(next["result"]["stopReason"], "end_turn", "{next}");
    assert_eq!(
        stub.generations().len(),
        2,
        "the next prompt reached the provider"
    );
    host.finish();
}

/// Catches: a host crash leaves the ACP run uncertain and rejects every fresh prompt
/// before the shared journal can settle the interrupted generation.
#[test]
fn crash_424_reloaded_acp_accepts_new_prompt_after_killed_generation() {
    let places = Places::new();
    let Reply::Paced { head, tail, .. } = paced("before crash", std::time::Duration::ZERO) else {
        unreachable!("paced produces a split reply");
    };
    let gate = ReleaseGate::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::GatedPaced {
                head,
                tail,
                gate: gate.clone(),
            },
            Reply::Sse(text("new answer after crash")),
        ],
        ..Plan::default()
    });
    std::fs::write(
        places.home_path().join("config.toml"),
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\n",
    )
    .expect("isolated ACP configuration");
    let running = Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
        .subcommand("acp")
        .spawn_conversing();
    let mut first = AcpHost::adopt(running);
    first.initialize();
    let session = first.new_session(places.workspace_path().to_str().expect("UTF-8 root"));
    first.send_request("session/prompt", prompt(&session));
    loop {
        let frame = first.read_frame();
        if frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
            break;
        }
    }
    first.child().kill().expect("kill only the owned ego child");
    gate.release();
    first.finish();

    let mut second = host(&places, &stub);
    second.initialize();
    let loaded = second.request(
        "session/load",
        serde_json::json!({
            "sessionId": session,
            "cwd": places.workspace_path(),
            "mcpServers": [],
        }),
    );
    assert!(loaded["result"].is_object(), "{loaded}");
    let answer = second.request("session/prompt", prompt(&session));
    second.finish();
    assert_eq!(
        answer["result"]["stopReason"], "end_turn",
        "a killed generation must not permanently wedge the ACP session: {answer}"
    );
    assert_eq!(stub.generations().len(), 2, "old request must not replay");
}

/// Catches: a silent ACP child bypasses the frame deadline on Windows.
#[test]
#[should_panic(expected = "the agent did not answer within")]
fn a_silent_owned_acp_child_is_terminated_at_the_frame_deadline() {
    let places = Places::new();
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .spawn_conversing(),
    );
    // No initialize request is sent: the real stdio agent has no frame to return.
    host.read_frame_with_deadline(std::time::Duration::from_millis(200));
}

/// Catches: EOF is reported as a frame timeout instead of a closed child.
#[test]
#[should_panic(expected = "the agent closed without answering")]
fn an_exited_owned_acp_child_reports_eof_without_waiting_for_the_deadline() {
    let places = Places::new();
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("acp")
            .spawn_conversing(),
    );
    host.child().kill().expect("kill only the owned ego child");
    host.child().wait().expect("reap the owned ego child");
    host.read_frame_with_deadline(std::time::Duration::from_secs(60));
}
