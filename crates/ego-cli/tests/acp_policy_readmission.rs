//! Saved policy recovery through real ego ACP processes and durable logs.
mod support;

use ego_core::store::codec::{StoreLine, decode_line, encode_record};
use serde_json::{Value, json};
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

fn prompt(host: &mut AcpHost, session: &str) -> u64 {
    host.send_request(
        "session/prompt",
        json!({
            "sessionId": session, "prompt": [{"type": "text", "text": "Say hello"}]
        }),
    )
}

fn save(places: &Places, stub: &Stub, sandbox: &str) -> String {
    std::fs::write(
        places.home_path().join("config.toml"),
        format!("[profile.chat]\nmodel='ollama/qwen3'\nthinking='off'\nsandbox='{sandbox}'\n"),
    )
    .unwrap();
    let mut first = host(places, stub);
    let session = first.new_session(places.workspace_path().to_str().unwrap());
    let id = prompt(&mut first, &session);
    loop {
        let frame = first.read_frame();
        if frame["id"].as_u64() == Some(id) {
            assert_eq!(frame["result"]["stopReason"], "end_turn", "{frame}");
            break;
        }
    }
    first.finish();
    session
}

// Keep real recorded state and its codec; simulate a prior digest schema only.
fn old_digest(places: &Places, session: &str, legacy: bool) {
    fn change(value: &mut Value, legacy: bool) -> usize {
        let mut count = 0;
        match value {
            Value::Object(object) => {
                if let Some(Value::Object(observation)) = object.get_mut("sandbox_observation") {
                    observation.insert("policy_digest".into(), json!("0".repeat(64)));
                    if legacy {
                        observation.remove("policy");
                    }
                    count += 1;
                }
                for child in object.values_mut() {
                    count += change(child, legacy);
                }
            }
            Value::Array(array) => {
                for child in array {
                    count += change(child, legacy);
                }
            }
            _ => {}
        }
        count
    }
    let path = places
        .home_path()
        .join("sessions")
        .join(session)
        .join("events.jsonl");
    let saved = std::fs::read_to_string(&path).unwrap();
    let mut output = Vec::new();
    let mut changed = 0;
    for line in saved.lines() {
        if let StoreLine::Record(record) = decode_line(line.as_bytes()).unwrap() {
            let mut payload: Value = serde_json::from_str(record.payload.get()).unwrap();
            changed += change(&mut payload, legacy);
            output.extend(encode_record(record.seq, &payload).unwrap());
        } else {
            output.extend(line.as_bytes());
        }
        output.push(b'\n');
    }
    assert!(changed > 0, "real session did not retain its admission");
    std::fs::write(path, output).unwrap();
}

// Catches: continuation reuses a stale digest and interrupts every resumed turn.
#[test]
fn older_digest_unknown_history_asks_permission_and_approval_resumes() {
    recover(true, true, false);
}

// Catches: lack of old roots being mistaken for consent on a denied recovery.
#[test]
fn older_digest_unknown_history_denial_starts_no_provider_effect() {
    recover(true, false, false);
}

// Catches: a digest schema change prompting even when semantic authority is equal.
#[test]
fn older_digest_equal_policy_recovers_automatically_and_survives_restart() {
    recover(false, true, false);
}

// Catches: current profile write roots widening saved confinement without a question.
#[test]
fn wider_profile_roots_require_permission_before_resumed_effects() {
    recover(false, true, true);
}

// Catches: a rejected wider profile being used by the resumed generation.
#[test]
fn wider_profile_roots_denial_starts_no_provider_effect() {
    recover(false, false, true);
}

fn recover(legacy: bool, allow: bool, wider: bool) {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let session = save(&places, &stub, if wider { "workspace" } else { "off" });
    let extra = tempfile::tempdir().unwrap();
    if wider {
        std::fs::write(places.home_path().join("config.toml"), format!(
            "[profile.chat]\nmodel='ollama/qwen3'\nthinking='off'\nsandbox='workspace'\nroots=[{{path='{}',access='read-write'}}]\n",
            extra.path().canonicalize().unwrap().display(),
        )).unwrap();
    } else {
        old_digest(&places, &session, legacy);
    }
    let mut resumed = host(&places, &stub);
    let loaded = resumed.request(
        "session/load",
        json!({
            "sessionId": session, "cwd": places.workspace_path(), "mcpServers": []
        }),
    );
    assert!(loaded["result"].is_object(), "{loaded}");
    let id = prompt(&mut resumed, &session);
    let mut asked = false;
    let mut noticed = false;
    loop {
        let frame = resumed.read_frame();
        if frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
            let text = frame["params"]["update"]["content"]["text"]
                .as_str()
                .unwrap_or_default();
            noticed |= text.contains("Execution policy changed:") && text.contains("policy digest");
        }
        if frame["method"] == "session/request_permission" {
            asked = true;
            resumed.write_frame(json!({"jsonrpc":"2.0", "id":frame["id"],
                "result":{"outcome":{"outcome":"selected", "optionId":if allow { "allow-once" } else { "reject-once" }}}}));
        }
        if frame["id"].as_u64() == Some(id) && frame.get("method").is_none() {
            assert_eq!(
                asked,
                legacy || wider,
                "wrong re-admission permission decision: {frame}"
            );
            assert!(noticed, "ACP did not name the changed policy");
            if allow {
                assert_eq!(frame["result"]["stopReason"], "end_turn", "{frame}");
            } else {
                assert!(
                    frame["error"].is_object(),
                    "denial must refuse the turn: {frame}"
                );
            }
            break;
        }
    }
    resumed.finish();
    assert_eq!(
        stub.generations().len(),
        if allow { 2 } else { 1 },
        "denied turn must have no provider effect"
    );
    let log = std::fs::read_to_string(
        places
            .home_path()
            .join("sessions")
            .join(&session)
            .join("events.jsonl"),
    )
    .unwrap();
    assert_eq!(
        log.contains("sandbox_readmission"),
        allow,
        "admission must be durable only after consent"
    );
    if allow {
        let mut third = host(&places, &stub);
        let loaded = third.request(
            "session/load",
            json!({
                "sessionId": session, "cwd": places.workspace_path(), "mcpServers": []
            }),
        );
        assert!(
            loaded["result"].is_object(),
            "readmission log must replay: {loaded}"
        );
        let id = prompt(&mut third, &session);
        loop {
            let frame = third.read_frame();
            assert_ne!(
                frame["method"], "session/request_permission",
                "recovery was not durable"
            );
            if frame["id"].as_u64() == Some(id) {
                assert_eq!(frame["result"]["stopReason"], "end_turn", "{frame}");
                break;
            }
        }
        third.finish();
        assert_eq!(stub.generations().len(), 3);
    }
}
