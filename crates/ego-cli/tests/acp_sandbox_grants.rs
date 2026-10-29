//! ACP sandbox denials through the real host: a command the sandbox refuses a
//! path outside the workspace becomes a question, and the answer decides
//! whether the command runs again (regression).

#![cfg(target_os = "macos")]

mod support;

use support::{
    AcpHost, Places, Plan, Reply, Stub, configure_permission_profile as configure,
    permission_host as host, text, tool_call,
};

const SECRET: &str = "outside-the-workspace-secret";

/// A file in a directory of its own, next to neither the workspace nor home.
fn outside_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("outside directory");
    let file = std::fs::canonicalize(dir.path())
        .expect("canonical outside directory")
        .join("notes.txt");
    std::fs::write(&file, format!("{SECRET}\n")).expect("outside file");
    (dir, file)
}

fn cat_call(id: &str, file: &std::path::Path) -> Reply {
    Reply::Sse(tool_call(
        id,
        "exec",
        &serde_json::json!({"argv": ["/bin/cat", file]}).to_string(),
    ))
}

fn send_prompt(host: &mut AcpHost, session: &str) -> u64 {
    host.send_request(
        "session/prompt",
        serde_json::json!({
            "sessionId": session,
            "prompt": [{"type": "text", "text": "Read the file"}],
        }),
    )
}

fn open_session(host: &mut AcpHost, places: &Places) -> String {
    host.initialize();
    host.new_session(places.workspace_path().to_str().expect("UTF-8 root"))
}

fn title(frame: &serde_json::Value) -> String {
    frame["params"]["toolCall"]["title"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn next_permission(host: &mut AcpHost) -> serde_json::Value {
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

/// Answer the ordinary exec question, fail on any sandbox question, and return
/// the prompt's response.
fn settle_without_sandbox_question(host: &mut AcpHost, prompt_id: u64) -> serde_json::Value {
    loop {
        let frame = host.read_frame();
        if frame["method"] == "session/request_permission" {
            assert!(
                !title(&frame).contains("sandbox refused"),
                "a granted path was asked again: {frame}"
            );
            select(host, &frame, "allow-once");
        }
        if frame["id"].as_u64() == Some(prompt_id) {
            return frame;
        }
    }
}

fn finish(host: &mut AcpHost, prompt_id: u64) -> serde_json::Value {
    loop {
        let frame = host.read_frame();
        if frame["id"].as_u64() == Some(prompt_id) {
            return frame;
        }
    }
}

fn sandbox_question(host: &mut AcpHost, file: &std::path::Path) -> serde_json::Value {
    let exec = next_permission(host);
    assert!(title(&exec).contains("exec("), "{exec}");
    select(host, &exec, "allow-once");
    let question = next_permission(host);
    let text = title(&question);
    assert!(text.contains("sandbox refused"), "{question}");
    assert!(
        text.contains("/bin/cat"),
        "the question did not show the command that will run again: {question}"
    );
    assert!(
        text.contains(&file.display().to_string()) && text.contains("read"),
        "the question did not name the path and the operation: {question}"
    );
    question
}

fn model_saw(stub: &Stub, needle: &str) -> bool {
    stub.generations()
        .iter()
        .skip(1)
        .any(|generation| generation.to_string().contains(needle))
}

#[test]
fn session_answer_runs_the_command_again_and_is_not_asked_twice_but_leaves_no_stored_grant() {
    // catches: an exec refusal with no way to widen the sandbox for this
    // session, a session answer that is asked again on the next prompt, and a
    // session answer that leaks into the user's stored grants.
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
    let session = open_session(&mut host, &places);

    let first = send_prompt(&mut host, &session);
    let question = sandbox_question(&mut host, &file);
    let ids: Vec<_> = question["params"]["options"]
        .as_array()
        .expect("options")
        .iter()
        .map(|option| option["optionId"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(ids, ["sandbox-session", "sandbox-always", "sandbox-deny"]);
    select(&mut host, &question, "sandbox-session");
    let finished = finish(&mut host, first);
    assert!(finished["result"].is_object(), "{finished}");
    assert!(
        model_saw(&stub, SECRET),
        "the command did not run again with the path allowed"
    );

    let second = send_prompt(&mut host, &session);
    let finished = settle_without_sandbox_question(&mut host, second);
    assert!(finished["result"].is_object(), "{finished}");
    host.finish();
    assert!(
        !places.home_path().join("sandbox-grants.json").exists(),
        "a session answer was stored for the workspace"
    );
}

#[test]
fn always_answer_is_stored_for_the_workspace_and_covers_the_next_session() {
    // catches: an "always for this workspace" answer that only lives in memory,
    // so the next ACP session asks about the same path again.
    let places = Places::new();
    configure(&places);
    let (_keep, file) = outside_file();
    let first_stub = Stub::start(Plan {
        completions: vec![cat_call("first", &file), Reply::Sse(text("first done"))],
        ..Plan::default()
    });
    let mut first = host(&places, &first_stub);
    let session = open_session(&mut first, &places);
    let prompt = send_prompt(&mut first, &session);
    let question = sandbox_question(&mut first, &file);
    select(&mut first, &question, "sandbox-always");
    let finished = finish(&mut first, prompt);
    assert!(finished["result"].is_object(), "{finished}");
    first.finish();

    let stored = std::fs::read_to_string(places.home_path().join("sandbox-grants.json"))
        .expect("the always answer was stored");
    assert!(stored.contains(&file.display().to_string()), "{stored}");
    assert!(stored.contains("read"), "{stored}");

    let second_stub = Stub::start(Plan {
        completions: vec![cat_call("again", &file), Reply::Sse(text("again done"))],
        ..Plan::default()
    });
    let mut second = host(&places, &second_stub);
    let session = open_session(&mut second, &places);
    let prompt = send_prompt(&mut second, &session);
    let finished = settle_without_sandbox_question(&mut second, prompt);
    assert!(finished["result"].is_object(), "{finished}");
    second.finish();
    assert!(
        model_saw(&second_stub, SECRET),
        "the stored grant did not reach the next session's command"
    );
}

#[test]
fn denied_answer_tells_the_model_the_sandbox_refused_and_does_not_run_the_command_again() {
    // catches: a deny that is reported as a bare `Operation not permitted`, or
    // that runs the command again anyway.
    let places = Places::new();
    configure(&places);
    let (_keep, file) = outside_file();
    let stub = Stub::start(Plan {
        completions: vec![cat_call("denied", &file), Reply::Sse(text("gave up"))],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    let session = open_session(&mut host, &places);
    let prompt = send_prompt(&mut host, &session);
    let question = sandbox_question(&mut host, &file);
    select(&mut host, &question, "sandbox-deny");
    let finished = finish(&mut host, prompt);
    assert!(finished["result"].is_object(), "{finished}");
    host.finish();

    assert!(
        model_saw(&stub, "sandbox denied read"),
        "the model was not told the sandbox refused the path"
    );
    assert!(!model_saw(&stub, SECRET), "a denied path was read anyway");
}
