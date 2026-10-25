mod support;

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::permit::ToolResult;
use ego_core::session_lifecycle::SessionLifecycleStore;
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{JsonlStore, OsIo};
use support::{Ego, Plan, Reply, Stub, text, tool_call};

// Catches: history tools are advertised but not bound to the admitted store or executed.
#[test]
fn history_tools_search_and_open_conversation_from_another_root() {
    let id = SessionId::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "search",
                "__ego_63bedf8acff0198465745adb1cc6ac86280c80864e92de652724ac7f73",
                r#"{"query":"acp trust"}"#,
            )),
            Reply::Sse(tool_call(
                "open",
                "__ego_499a3ed2db8204841d4cae8578f638ac459d83325d9d2723233e010bf9",
                &format!(r#"{{"session":"{id}"}}"#),
            )),
            Reply::Sse(text("History was available.")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub);
    let root = ego.workspace_path().join("previous-root");
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let store = JsonlStore::new(
        ego.home_path().join("sessions"),
        root.clone(),
        Box::new(OsIo),
    );
    store
        .publish_empty_session(id, &root, Timestamp::from_millis(1))
        .unwrap();
    let turn = TurnId::new();
    let mut seq = Seq::ZERO;
    for (item, kind) in [
        (
            None,
            EventKind::SessionTitleChanged {
                title: Some("ACP trust notes".into()),
            },
        ),
        (
            Some(ItemId::new()),
            EventKind::UserPrompt {
                audio: Vec::new(),
                text: "acp trust user observation".into(),
                images: Vec::new(),
                queued_prompt: None,
            },
        ),
        (
            Some(ItemId::new()),
            EventKind::AssistantMessage {
                text: "assistant durable answer".into(),
            },
        ),
        (
            Some(ItemId::new()),
            EventKind::ToolCallCompleted {
                ok: true,
                result: ToolResult::builtin("RAW-TOOL-MUST-NOT-LEAK"),
            },
        ),
        (
            None,
            EventKind::TurnCompleted {
                ok: true,
                exit_reason: ExitReason::Completed,
            },
        ),
    ] {
        store
            .append(
                &id,
                &DomainEvent {
                    env: Envelope {
                        seq: seq.next(),
                        id: EventId::new(),
                        turn,
                        item,
                        parent: None,
                        ts: Timestamp::from_millis(1000),
                        producer: Producer::Main,
                    },
                    kind,
                },
            )
            .unwrap();
    }
    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--tools",
            "history.search,history.open",
            "Find the previous ACP decision",
        ])
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    assert_eq!(finished.stdout.trim(), "History was available.");
    let requests = stub.generations();
    assert_eq!(requests.len(), 3);
    let results = |index: usize| {
        requests[index]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .filter_map(|message| message["content"].as_str())
            .collect::<Vec<_>>()
    };
    let search = results(1);
    assert_eq!(search.len(), 1);
    assert!(search[0].len() <= 1024);
    let hits: serde_json::Value = serde_json::from_str(search[0]).unwrap();
    assert_eq!(hits["hits"][0]["id"], id.to_string());
    assert_eq!(hits["hits"][0]["title"], "ACP trust notes");
    let opened = results(2);
    let conversation = opened.last().unwrap();
    assert!(conversation.contains("user: acp trust user observation"));
    assert!(conversation.contains("assistant: assistant durable answer"));
    assert!(!conversation.contains("RAW-TOOL-MUST-NOT-LEAK"));
}
