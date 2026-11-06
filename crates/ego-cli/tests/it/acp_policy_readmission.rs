//! Saved policy recovery through real ego ACP processes and durable logs.
use crate::support;

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

// Catches: the ACP measurement ladder replaces admitted workspace authority
// with the off ceiling, allowing a headless CLI widening without consent.
#[test]
fn acp_load_ceiling_does_not_authorize_wider_cli_resume() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let session = save(&places, &stub, "workspace");
    std::fs::write(
        places.home_path().join("config.toml"),
        "[profile.chat]\nmodel='ollama/qwen3'\nthinking='off'\nsandbox='off'\n",
    )
    .unwrap();
    let mut loaded = host(&places, &stub);
    let response = loaded.request(
        "session/load",
        json!({"sessionId": session, "cwd": places.workspace_path(), "mcpServers": []}),
    );
    assert!(response["result"].is_object(), "{response}");
    loaded.finish();

    let resumed = Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
        .subcommand("resume")
        .args([
            session.as_str(),
            "--profile",
            "chat",
            "--output",
            "text",
            "next",
        ])
        .run();
    assert_eq!(resumed.code, Some(3), "{}", resumed.stderr);
    assert_eq!(
        stub.generations().len(),
        1,
        "unapproved widening reached provider"
    );
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

// Catches: rejecting widened saved authority still starts a provider generation.
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
            if text.contains("Execution policy changed:") {
                assert!(
                    !text.contains("policy digest"),
                    "ACP must not restore opaque digest diagnostics: {text}"
                );
                if wider {
                    assert!(text.contains("filesystem root added"), "{text}");
                    assert!(
                        text.contains(extra.path().canonicalize().unwrap().to_str().unwrap()),
                        "ACP must name the added root: {text}"
                    );
                } else if legacy {
                    assert!(
                        text.contains("historical roots/policy cannot be reconstructed"),
                        "{text}"
                    );
                }
                noticed = true;
            }
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

// Catches: a refused ACP widening is journaled as the session authority, so a later
// CLI resume under the same wider authority finds an equal measurement and runs unasked.
#[test]
fn refused_acp_widening_still_requires_consent_on_cli_resume() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let session = save(&places, &stub, "workspace");
    let extra = tempfile::tempdir().unwrap();
    std::fs::write(
        places.home_path().join("config.toml"),
        format!(
            "[profile.chat]\nmodel='ollama/qwen3'\nthinking='off'\nsandbox='workspace'\nroots=[{{path='{}',access='read-write'}}]\n",
            extra.path().canonicalize().unwrap().display(),
        ),
    )
    .unwrap();
    let mut refused = host(&places, &stub);
    let loaded = refused.request(
        "session/load",
        json!({
            "sessionId": session, "cwd": places.workspace_path(), "mcpServers": []
        }),
    );
    assert!(loaded["result"].is_object(), "{loaded}");
    let id = prompt(&mut refused, &session);
    let mut asked = false;
    loop {
        let frame = refused.read_frame();
        if frame["method"] == "session/request_permission" {
            asked = true;
            refused.write_frame(json!({"jsonrpc":"2.0", "id":frame["id"],
                "result":{"outcome":{"outcome":"selected", "optionId":"reject-once"}}}));
        }
        if frame["id"].as_u64() == Some(id) && frame.get("method").is_none() {
            assert!(asked, "ACP did not ask about the wider roots: {frame}");
            assert!(
                frame["error"].is_object(),
                "denial must refuse the turn: {frame}"
            );
            break;
        }
    }
    refused.finish();
    assert_eq!(stub.generations().len(), 1);

    let resumed = Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
        .subcommand("resume")
        .args([
            session.as_str(),
            "--profile",
            "chat",
            "--output",
            "text",
            "next",
        ])
        .run();
    assert_ne!(
        resumed.code,
        Some(0),
        "headless CLI resume ran under the authority ACP refused: {}",
        resumed.stderr
    );
    assert_eq!(
        stub.generations().len(),
        1,
        "the refused wider authority reached the provider through the CLI"
    );
}

fn widen_roots(places: &Places) -> tempfile::TempDir {
    let extra = tempfile::tempdir().unwrap();
    std::fs::write(places.home_path().join("config.toml"), format!(
        "[profile.chat]\nmodel='ollama/qwen3'\nthinking='off'\nsandbox='workspace'\nroots=[{{path='{}',access='read-write'}}]\n",
        extra.path().canonicalize().unwrap().display(),
    )).unwrap();
    extra
}

fn headless_resume(places: &Places, stub: &Stub, session: &str, accept: bool) -> support::Finished {
    let ego = Ego::at(places.home_path(), places.workspace_path(), Some(stub)).subcommand("resume");
    if accept {
        ego.args([
            session,
            "--profile",
            "chat",
            "--output",
            "text",
            "--accept-authority-delta",
            "next",
        ])
    } else {
        ego.args([session, "--profile", "chat", "--output", "text", "next"])
    }
    .run()
}

// Catches: the owner deliberately changes roots but cannot resume a headless saved job.
#[test]
fn headless_resume_accepts_explicit_measured_authority_delta() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let session = save(&places, &stub, "workspace");
    let _extra = widen_roots(&places);

    let accepted = headless_resume(&places, &stub, &session, true);
    assert_eq!(accepted.code, Some(0), "{}", accepted.stderr);
    assert!(
        accepted.stderr.contains("Execution policy changed:"),
        "the accepted delta must be shown: {}",
        accepted.stderr
    );
    assert_eq!(stub.generations().len(), 2, "accepted resume did not run");

    // The accepted measurement became the saved authority: no flag, no question.
    let again = headless_resume(&places, &stub, &session, false);
    assert_eq!(again.code, Some(0), "{}", again.stderr);
    assert_eq!(stub.generations().len(), 3);
}

// Catches: downloaded config silently widens saved authority, or the option approves
// a later, different delta than the one shown to the owner on this resume.
#[test]
fn unaccepted_or_ceiling_exceeding_resume_delta_is_refused() {
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let session = save(&places, &stub, "workspace");
    let _extra = widen_roots(&places);

    let refused = headless_resume(&places, &stub, &session, false);
    assert_eq!(refused.code, Some(3), "{}", refused.stderr);
    assert_eq!(stub.generations().len(), 1, "unaccepted widening ran");

    // The option is per resume: a refused run left no standing approval behind.
    let still = headless_resume(&places, &stub, &session, false);
    assert_eq!(still.code, Some(3), "{}", still.stderr);
    assert_eq!(stub.generations().len(), 1);
}
