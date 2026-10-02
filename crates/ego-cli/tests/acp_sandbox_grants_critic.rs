//! Scope of an ACP "allow for this session" sandbox answer: it belongs to one
//! open session, not to the connection, the process, or a session id that was
//! closed and opened again.

#![cfg(target_os = "macos")]

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

fn outside_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("outside directory");
    let file = std::fs::canonicalize(dir.path())
        .expect("canonical outside directory")
        .join("notes.txt");
    std::fs::write(&file, "outside\n").expect("outside file");
    (dir, file)
}

fn cat_call(id: &str, file: &std::path::Path) -> Reply {
    Reply::Sse(tool_call(
        id,
        "exec",
        &serde_json::json!({"argv": ["/bin/cat", file]}).to_string(),
    ))
}

fn host(places: &Places, stub: &Stub) -> AcpHost {
    AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(stub))
            .subcommand("acp")
            .args(["--profile", PROFILE])
            .spawn_conversing(),
    )
}

fn prompt(host: &mut AcpHost, session: &str) -> u64 {
    host.send_request(
        "session/prompt",
        serde_json::json!({
            "sessionId": session,
            "prompt": [{"type": "text", "text": "Read the file"}],
        }),
    )
}

fn title(frame: &serde_json::Value) -> String {
    frame["params"]["toolCall"]["title"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn answer(host: &mut AcpHost, request: &serde_json::Value, option: &str) {
    host.write_frame(serde_json::json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": {"outcome": {"outcome": "selected", "optionId": option}},
    }));
}

/// Answer the ordinary exec question with allow-once, then return the sandbox
/// question if the prompt raises one; `None` when the prompt ends without it.
fn sandbox_question(host: &mut AcpHost, prompt_id: u64) -> Option<serde_json::Value> {
    loop {
        let frame = host.read_frame();
        if frame["method"] == "session/request_permission" {
            if title(&frame).contains("sandbox refused") {
                return Some(frame);
            }
            answer(host, &frame, "allow-once");
        }
        if frame["id"].as_u64() == Some(prompt_id) {
            return None;
        }
    }
}

fn finish(host: &mut AcpHost, prompt_id: u64) {
    loop {
        if host.read_frame()["id"].as_u64() == Some(prompt_id) {
            return;
        }
    }
}

/// Catches: a session answer is kept in a map owned by the connection and keyed
/// by session id, never dropped at `session/close`: closing a session and
/// opening the same id again (`session/load`, which resets the exec session
/// approval) keeps the old session's reach, so the person is not asked again.
#[test]
fn a_session_answer_ends_when_the_session_is_closed_and_loaded_again() {
    let places = Places::new();
    configure(&places);
    let (_keep, file) = outside_file();
    let stub = Stub::start(Plan {
        completions: vec![
            cat_call("first", &file),
            Reply::Sse(text("first done")),
            cat_call("second", &file),
            Reply::Sse(text("second done")),
        ],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    host.initialize();
    let workspace = places.workspace_path().to_str().expect("UTF-8 root");
    let session = host.new_session(workspace);

    let first = prompt(&mut host, &session);
    let question = sandbox_question(&mut host, first).expect("the first read is refused");
    answer(&mut host, &question, "sandbox-session");
    finish(&mut host, first);

    let closed = host.request("session/close", serde_json::json!({"sessionId": session}));
    assert!(closed["result"].is_object(), "{closed}");
    let loaded = host.request(
        "session/load",
        serde_json::json!({"sessionId": session, "cwd": workspace, "mcpServers": []}),
    );
    assert!(loaded["result"].is_object(), "{loaded}");

    let second = prompt(&mut host, &session);
    let asked_again = sandbox_question(&mut host, second);
    assert!(
        asked_again.is_some(),
        "the session answer outlived the session it was given in"
    );
}

/// Catches: a session answer given in one session covers a different session of
/// the same connection and workspace.
#[test]
fn a_session_answer_does_not_cover_another_session_of_the_same_connection() {
    let places = Places::new();
    configure(&places);
    let (_keep, file) = outside_file();
    let stub = Stub::start(Plan {
        completions: vec![
            cat_call("first", &file),
            Reply::Sse(text("first done")),
            cat_call("other", &file),
            Reply::Sse(text("other done")),
        ],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    host.initialize();
    let workspace = places.workspace_path().to_str().expect("UTF-8 root");
    let one = host.new_session(workspace);
    let two = host.new_session(workspace);

    let first = prompt(&mut host, &one);
    let question = sandbox_question(&mut host, first).expect("the first read is refused");
    answer(&mut host, &question, "sandbox-session");
    finish(&mut host, first);

    let other = prompt(&mut host, &two);
    assert!(
        sandbox_question(&mut host, other).is_some(),
        "session two read a path that only session one was allowed"
    );
}
