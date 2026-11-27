use super::*;
use ego_core::event::id::SessionId;
use ego_core::event::time::Timestamp;
use ego_core::session_lifecycle::SessionLifecycleStore;
use ego_tools::store::{JsonlStore, Lease, OsIo};

// Catches: a rejected candidate closes the current coordinator and strands /quit.
#[test]
fn picking_a_session_with_a_held_writer_lease_returns_to_the_prompt_and_quit_works() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    places.file("AGENTS.md", "# resume writer lease fixture\n");
    let root = std::fs::canonicalize(places.workspace_path()).unwrap();
    let sessions = places.home_path().join("sessions");
    let store = JsonlStore::new(&sessions, &root, Box::new(OsIo));
    let id = SessionId::new();
    store
        .publish_empty_session(id, &root, Timestamp::from_millis(1000))
        .unwrap();
    let _lease = Lease::acquire(
        &sessions.join(id.to_string()).join("lease"),
        std::process::id(),
        Timestamp::from_millis(1000),
        Duration::from_secs(60),
        &|_| true,
    )
    .unwrap();
    let ego = places
        .ego(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input
        .write_all(format!("/resume {id}\r").as_bytes())
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "writer lease already held");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success(), "{}", normalized(&output));
    assert!(
        !normalized(&output).contains("the interactive turn coordinator is closed"),
        "{}",
        normalized(&output)
    );
}

// Catches: per-session errors reaching the transcript, or /quit being swallowed by the picker.
#[test]
fn resume_picker_skips_unreadable_sessions_without_notes() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    places.file("AGENTS.md", "# resume unreadable fixture\n");
    let root = std::fs::canonicalize(places.workspace_path()).unwrap();
    let sessions = places.home_path().join("sessions");
    let store = JsonlStore::new(&sessions, &root, Box::new(OsIo));
    let ids: Vec<_> = (0..4).map(|_| SessionId::new()).collect();
    for id in &ids {
        store
            .publish_empty_session(*id, &root, Timestamp::from_millis(1000))
            .unwrap();
    }
    std::fs::write(
        sessions.join(ids[0].to_string()).join("manifest.json"),
        "invalid manifest",
    )
    .unwrap();
    std::fs::remove_file(sessions.join(ids[1].to_string()).join("events.jsonl")).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(sessions.join(ids[2].to_string()).join("events.jsonl"))
        .unwrap()
        .write_all(b"{\"torn\":")
        .unwrap();
    std::fs::create_dir(sessions.join(SessionId::new().to_string())).unwrap();
    let ego = places
        .ego(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"/resume\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Untitled");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success(), "{}", normalized(&output));
    let transcript = normalized(&output[start..]);
    assert!(!transcript.contains("is unreadable"), "{transcript}");
    assert!(
        !transcript.contains("incompatible sessions"),
        "{transcript}"
    );
    assert!(
        !transcript.contains("coordinator is closed"),
        "{transcript}"
    );
}

// Catches: Page Down reopens the first page, skips rows, or only pages the visible viewport.
#[test]
fn resume_picker_page_down_loads_the_next_ten_and_selects_that_page() {
    use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
    use ego_core::event::id::{EventId, Producer, Seq, TurnId};
    use ego_core::store::session_store::SessionStore;
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    places.file("AGENTS.md", "# resume page fixture\n");
    let root = std::fs::canonicalize(places.workspace_path()).unwrap();
    let sessions = places.home_path().join("sessions");
    let store = JsonlStore::new(&sessions, &root, Box::new(OsIo));
    let ids: Vec<_> = (0..12).map(|_| SessionId::new()).collect();
    for (index, id) in ids.iter().enumerate() {
        let stamp = Timestamp::from_millis((index as u64 + 1) * 1000);
        store.publish_empty_session(*id, &root, stamp).unwrap();
        let mut seq = Seq::ZERO;
        store
            .append(
                id,
                &DomainEvent {
                    env: Envelope {
                        seq: seq.next(),
                        id: EventId::new(),
                        turn: TurnId::new(),
                        item: None,
                        parent: None,
                        ts: stamp,
                        producer: Producer::Main,
                    },
                    kind: EventKind::SessionTitleChanged {
                        title: Some(format!("PAGE-{:02}", index + 1)),
                    },
                },
            )
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(sessions.join(id.to_string()).join("events.jsonl"))
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(index as u64 + 1)),
            )
            .unwrap();
    }
    let ego = places
        .ego(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    let first = output.len();
    input.write_all(b"/resume\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, first, "PAGE-12");
    let next = output.len();
    input.write_all(b"\x1b[6~").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, next, "PAGE-02");
    until_normalized(&reader, &mut output, next, "PAGE-01");
    assert!(!normalized(&output[next..]).contains("PAGE-12"));
    input.write_all(b"page-01").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, next, "filter: page-01");
    let chosen = output.len();
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        chosen,
        &format!("resumed session {}", ids[0]),
    );
    until_prompt_from(&reader, &mut output, chosen);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success(), "{}", normalized(&output));
}

// Catches: a same-root picker journals source argv tools without rebuilding the target catalogue.
#[test]
fn same_root_picker_restores_target_tools_and_retains_the_active_model() {
    use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
    use ego_core::event::id::{EventId, Producer, Seq, TurnId};
    use ego_core::run::RunSelections;
    use ego_core::store::session_store::SessionStore;

    let stub = Stub::start(Plan {
        installed: vec!["qwen3".to_owned(), "other".to_owned()],
        context_length: Some(32_768),
        completions: vec![Reply::Sse(support::text("PICKER-TOOLS-RESTORED"))],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# picker selection fixture\n");
    let root = std::fs::canonicalize(places.workspace_path()).unwrap();
    let sessions = places.home_path().join("sessions");
    let store = JsonlStore::new(&sessions, &root, Box::new(OsIo));
    let id = SessionId::new();
    let stamp = Timestamp::from_millis(1000);
    store.publish_empty_session(id, &root, stamp).unwrap();
    let mut seq = Seq::ZERO;
    store
        .append(
            &id,
            &DomainEvent {
                env: Envelope {
                    seq: seq.next(),
                    id: EventId::new(),
                    turn: TurnId::new(),
                    item: None,
                    parent: None,
                    ts: stamp,
                    producer: Producer::Main,
                },
                kind: EventKind::SessionSelectionsSelected {
                    selections: RunSelections::empty()
                        .with_model_and_thinking(
                            ego_core::config::ModelRef("ollama/other".to_owned()),
                            ego_core::generator::types::ThinkingLevel::Off,
                        )
                        .with_tools(Some(vec!["web_search_external".to_owned()])),
                },
            },
        )
        .unwrap();
    let ego = places
        .ego(&stub)
        .env("EXA_API_KEY", "unused-picker-fixture-key")
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--tools",
            "read",
        ]);
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input
        .write_all(format!("/resume {id}\r").as_bytes())
        .unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        &format!("resumed session {id}"),
    );
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input
        .write_all(b"Reply with the fixture marker.\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "PICKER-TOOLS-RESTORED");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success(), "{}", normalized(&output));
    let requests = stub.generations();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0]["model"], "qwen3");
    let names: Vec<_> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert_eq!(names, vec!["web_search_external"], "{}", requests[0]);
    let resumed = places
        .ego(&stub)
        .env("EXA_API_KEY", "unused-picker-fixture-key")
        .subcommand("resume")
        .args([&id.to_string(), "Reply again."])
        .run();
    assert_eq!(resumed.code, Some(0), "{}", resumed.stderr);
    let requests = stub.generations();
    assert_eq!(requests.len(), 2);
    let names: Vec<_> = requests[1]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert_eq!(names, vec!["web_search_external"], "{}", requests[1]);
}
