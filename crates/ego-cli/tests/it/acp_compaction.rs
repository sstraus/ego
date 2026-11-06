//! An ACP prompt compacts and continues instead of stopping, and
//! the client's next prompt on the same session id reaches the successor.
//!
//! Every test runs `ego acp` as a process against the stub provider. The stub
//! reports a 272000-token window, so the 90% threshold is 244800.

use crate::support;

use serde_json::Value;
use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text, tool_call};

const CHECKPOINT: &str = "# Objective\nCHECKPOINT-MARKER\n# Binding constraints and preferences\nNone.\n# Completed work and evidence\nRead notes.md.\n# Work in progress\nFinish the task.\n# Decisions and rationale\nNone.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nAnswer.\n# Critical references\nnotes.md";

/// A read tool call whose usage reports `prompt_tokens` of measured occupancy.
fn measured_read(prompt_tokens: u64) -> Reply {
    Reply::Sse(
        tool_call("call_850", "read", "{\"files\":[{\"path\":\"notes.md\"}]}").replace(
            "\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14",
            &format!(
                "\"prompt_tokens\":{prompt_tokens},\"completion_tokens\":4,\"total_tokens\":{}",
                prompt_tokens + 4
            ),
        ),
    )
}

/// The capability a client sends to accept the `_meta.ego.compacted` update.
fn accepts_transitions() -> Value {
    serde_json::json!({"_meta": {"ego": {"compacted": {"version": 1}}}})
}

fn host(places: &Places, completions: Vec<Reply>) -> (AcpHost, Stub, String) {
    host_accepting(places, completions, accepts_transitions())
}

fn host_accepting(
    places: &Places,
    completions: Vec<Reply>,
    client_capabilities: Value,
) -> (AcpHost, Stub, String) {
    let stub = Stub::start(Plan {
        completions,
        context_length: Some(272_000),
        ..Plan::default()
    });
    std::fs::write(
        places.home_path().join("config.toml"),
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"workspace\"\nmode = \"default\"\n",
    )
    .unwrap();
    std::fs::write(places.workspace_path().join("notes.md"), "the contents").unwrap();
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand("acp")
            .spawn_conversing(),
    );
    let initialized = host.initialize_with(client_capabilities);
    assert_eq!(
        initialized["result"]["agentCapabilities"]["_meta"]["ego"]["compacted"],
        serde_json::json!({"version": 1, "update": "session_info_update", "field": "_meta.ego.compacted"}),
        "{initialized}"
    );
    let session = host.new_session(places.workspace_path().to_str().unwrap());
    (host, stub, session)
}

/// Send one prompt and read until its answer, granting any permission asked.
fn prompt(host: &mut AcpHost, session: &str, text: &str) -> (Value, Vec<Value>) {
    let id = host.send_request(
        "session/prompt",
        serde_json::json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
    );
    let mut frames = Vec::new();
    loop {
        let frame = host.read_frame();
        if frame["id"] == id && frame.get("method").is_none() {
            return (frame, frames);
        }
        if frame["method"] == "session/request_permission" {
            host.write_frame(serde_json::json!({"jsonrpc": "2.0", "id": frame["id"],
                "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}}}));
        }
        frames.push(frame);
    }
}

fn compacted_updates(frames: &[Value]) -> Vec<&Value> {
    frames
        .iter()
        .filter(|frame| frame["method"] == "session/update")
        .filter_map(|frame| {
            frame["params"]["update"]["_meta"]["ego"]
                .get("compacted")
                .map(|compacted| (frame, compacted))
        })
        .map(|(frame, compacted)| {
            assert_eq!(
                frame["params"]["update"]["sessionUpdate"],
                "session_info_update"
            );
            compacted
        })
        .collect()
}

fn request_contains(request: &Value, expected: &str) -> bool {
    request["messages"]
        .as_array()
        .expect("provider request has messages")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .any(|content| content.contains(expected))
}

fn answered_text(frames: &[Value]) -> String {
    frames
        .iter()
        .filter(|frame| frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
        .collect()
}

/// Run one prompt past the threshold and return the successor id.
fn compact_through_first_prompt(host: &mut AcpHost, session: &str) -> String {
    let (answer, frames) = prompt(host, session, "Read notes.md");
    assert_eq!(
        answer["result"]["stopReason"], "end_turn",
        "the prompt must continue in the successor: {answer}"
    );
    let compacted = compacted_updates(&frames);
    assert_eq!(compacted.len(), 1, "one transition update: {frames:?}");
    let compacted = compacted[0];
    assert_eq!(compacted["version"], 1);
    assert_eq!(compacted["sourceSessionId"], session);
    assert!(compacted["sourceSeq"].as_u64().is_some(), "{compacted}");
    let target = compacted["targetSessionId"]
        .as_str()
        .expect("target session id")
        .to_owned();
    assert_ne!(target, session);
    // The update names the client's session id, so a client that ignores it
    // still renders the continuation in the conversation it shows.
    let position = frames
        .iter()
        .position(|frame| {
            frame["params"]["update"]["_meta"]["ego"]
                .get("compacted")
                .is_some()
        })
        .unwrap();
    assert!(
        frames[position..]
            .iter()
            .filter(|frame| frame["method"] == "session/update")
            .all(|frame| frame["params"]["sessionId"] == session)
    );
    assert!(
        answered_text(&frames[position..]).contains("RECOVERED-MARK"),
        "{frames:?}"
    );
    target
}

// Catches: an ACP session that reaches the threshold stops with
// _context_overflow, or the client's next prompt on its session id reaches the
// uncompacted source instead of the successor.
#[test]
fn an_acp_prompt_at_the_threshold_continues_in_the_successor_and_the_next_prompt_follows() {
    let places = Places::new();
    let (mut host, stub, session) = host(
        &places,
        vec![
            measured_read(280_000),
            Reply::Sse(text(CHECKPOINT)),
            Reply::Sse(text("RECOVERED-MARK")),
            Reply::Sse(text("SECOND-MARK")),
        ],
    );
    compact_through_first_prompt(&mut host, &session);
    let (answer, frames) = prompt(&mut host, &session, "And now?");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert!(answered_text(&frames).contains("SECOND-MARK"), "{frames:?}");
    assert!(compacted_updates(&frames).is_empty());
    let requests = stub.generations();
    assert_eq!(
        requests.len(),
        4,
        "tool turn, checkpoint, continuation, next prompt"
    );
    // The source tail (its tool result) must not reach the successor, and the
    // next prompt must carry the checkpoint, not the source conversation.
    for successor in &requests[2..] {
        assert!(request_contains(successor, "CHECKPOINT-MARKER"));
        assert!(
            !request_contains(successor, "the contents"),
            "a successor request carries the source tool result"
        );
    }
    host.finish();
}

// Catches: an explicit _ego/compact that names the client's session id after
// an automatic compaction compacts the stale source and drops the successor's work.
#[test]
fn explicit_compaction_after_an_automatic_one_compacts_the_successor() {
    let places = Places::new();
    let (mut host, _stub, session) = host(
        &places,
        vec![
            measured_read(280_000),
            Reply::Sse(text(CHECKPOINT)),
            Reply::Sse(text("RECOVERED-MARK")),
            Reply::Sse(text(CHECKPOINT)),
        ],
    );
    let target = compact_through_first_prompt(&mut host, &session);
    let compacted = host.request(
        "_ego/compact",
        serde_json::json!({"v": 1, "sessionId": session,
            "requestId": "0193a7d0-0000-7000-8000-000000000850"}),
    );
    assert_eq!(
        compacted["result"]["sourceSessionId"], target,
        "{compacted}"
    );
    host.finish();
}

// Catches: a failed ACP compaction answers end_turn, spins on the provider,
// or hides the overflow error that the client's explicit fallback reacts to.
#[test]
fn a_failed_acp_compaction_answers_the_typed_overflow() {
    let places = Places::new();
    let (mut host, stub, session) = host(&places, vec![measured_read(280_000), Reply::Status(400)]);
    let (answer, frames) = prompt(&mut host, &session, "Read notes.md");
    // v1 has no overflow stop reason; the client's fallback reads this error.
    assert!(
        answer["error"]
            .to_string()
            .contains("the provider context window is exhausted; invoke _ego/compact"),
        "{answer}"
    );
    assert!(compacted_updates(&frames).is_empty(), "{frames:?}");
    assert_eq!(stub.generations().len(), 2, "failed recovery must not spin");
    host.finish();
}

// Catches: the successor of an automatic compaction forgets the session's
// ceilingProfile, so the next prompt on the client's session id runs at the
// launch confinement (off) instead of the clamped one (workspace).
#[test]
fn the_next_prompt_after_an_automatic_compaction_keeps_the_session_ceiling() {
    use ego_core::evidence::{EvidenceLogSource, EvidenceRecord};
    use ego_tools::store::{JsonlStore, OsIo};
    let places = Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "model='ollama/qwen3'\nthinking='off'\n[profile.machine]\nmode='default'\nsandbox='workspace'\nroots=[]\n[profile.wide]\nmode='yolo'\nsandbox='off'\nroots=[]\n",
    )
    .unwrap();
    std::fs::write(places.workspace_path().join("notes.md"), "the contents").unwrap();
    let stub = Stub::start(Plan {
        context_length: Some(272_000),
        completions: vec![
            measured_read(280_000),
            Reply::Sse(text(CHECKPOINT)),
            Reply::Sse(text("RECOVERED-MARK")),
            Reply::Sse(text("SECOND-MARK")),
        ],
        ..Plan::default()
    });
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand("acp")
            .args(["--profile", "wide", "--sandbox", "off"])
            .spawn_conversing(),
    );
    host.initialize_with(accepts_transitions());
    let opened = host.request(
        "session/new",
        serde_json::json!({"cwd": places.workspace_path(), "mcpServers": [],
            "_meta": {"ego": {"profile": "wide", "ceilingProfile": "machine", "trust": "trusted"}}}),
    );
    let session = opened["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("{opened}"))
        .to_owned();
    let target = compact_through_first_prompt(&mut host, &session);
    let (answer, _) = prompt(&mut host, &session, "And now?");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    host.finish();
    let confinements: Vec<String> = JsonlStore::new(
        places.home_path().join("sessions"),
        places.workspace_path(),
        Box::new(OsIo),
    )
    .evidence_records(&target.parse().unwrap())
    .unwrap()
    .into_iter()
    .filter_map(|record| match record {
        EvidenceRecord::RunContext(context) => {
            Some(context.execution().confinement().as_str().to_owned())
        }
        _ => None,
    })
    .collect();
    assert!(
        !confinements.is_empty(),
        "the successor ran the second prompt"
    );
    assert!(
        confinements
            .iter()
            .all(|confinement| confinement == "workspace"),
        "the successor widened past the session ceiling: {confinements:?}"
    );
}

// Catches: an ACP client that did not accept `_meta.ego.compacted` is compacted
// behind its back to a session id it cannot follow, instead of receiving the
// typed overflow error that points at explicit `_ego/compact`.
#[test]
fn a_client_without_the_transition_capability_keeps_the_typed_overflow() {
    let places = Places::new();
    let (mut host, stub, session) = host_accepting(
        &places,
        vec![measured_read(280_000), Reply::Sse(text(CHECKPOINT))],
        serde_json::json!({}),
    );
    let (answer, frames) = prompt(&mut host, &session, "Read notes.md");
    assert!(
        answer["error"]
            .to_string()
            .contains("the provider context window is exhausted; invoke _ego/compact"),
        "{answer}"
    );
    assert!(compacted_updates(&frames).is_empty(), "{frames:?}");
    assert_eq!(stub.generations().len(), 1, "no automatic compaction ran");
    host.finish();
}
