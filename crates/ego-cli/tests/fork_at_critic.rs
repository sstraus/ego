//! Independent critic cases for the public message fork surface.

mod support;

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, ExitReason};
use ego_core::event::id::{EventId, ItemId, Producer, QueuedPromptId, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::types::{ContentBlock, Role};
use ego_core::session::{ReplayContext, ReplaySource};
use ego_core::session_lifecycle::SessionLifecycleStore;
use ego_core::state::SessionState;
use ego_core::store::session_store::{SessionEntry, SessionStore};
use ego_tools::store::{JsonlStore, OsIo};

// Catches: reusing the idle-tip queue guard refuses a completed message whose
// next prompt was accepted during generation, even after that prompt ran.
#[test]
fn completed_message_fork_does_not_require_the_source_queue_to_have_been_empty() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(workspace.path()).unwrap();
    let store = JsonlStore::new(sessions.path(), &root, Box::new(OsIo));
    let source = SessionId::new();
    store
        .publish_empty_session(source, &root, Timestamp::from_millis(1))
        .unwrap();
    let turn = TurnId::new();
    let next_turn = TurnId::new();
    let answer = ItemId::new();
    let queued = QueuedPromptId::new();
    let mut seq = Seq::ZERO;
    // This order comes from turn::apply_intent: Submit durably accepts input
    // while the current turn is active; the next turn consumes it afterwards.
    for (turn, item, kind) in [
        (
            turn,
            Some(ItemId::new()),
            EventKind::UserPrompt {
                audio: Vec::new(),
                text: "first question".into(),
                images: vec![],
                queued_prompt: None,
            },
        ),
        (
            turn,
            None,
            EventKind::PromptQueued {
                audio: Vec::new(),
                id: queued,
                text: "second question".into(),
                images: vec![],
            },
        ),
        (
            turn,
            Some(answer),
            EventKind::AssistantMessage {
                text: "first answer".into(),
            },
        ),
        (
            turn,
            None,
            EventKind::TurnCompleted {
                ok: true,
                exit_reason: ExitReason::Completed,
            },
        ),
        (
            next_turn,
            Some(ItemId::new()),
            EventKind::UserPrompt {
                audio: Vec::new(),
                text: "second question".into(),
                images: vec![],
                queued_prompt: Some(queued),
            },
        ),
        (
            next_turn,
            Some(ItemId::new()),
            EventKind::AssistantMessage {
                text: "second answer".into(),
            },
        ),
        (
            next_turn,
            None,
            EventKind::TurnCompleted {
                ok: true,
                exit_reason: ExitReason::Completed,
            },
        ),
    ] {
        store
            .append(
                &source,
                &DomainEvent {
                    env: Envelope {
                        seq: seq.next(),
                        id: EventId::new(),
                        turn,
                        item,
                        parent: None,
                        ts: Timestamp::from_millis(10),
                        producer: Producer::Main,
                    },
                    kind,
                },
            )
            .unwrap();
    }
    let before = store.entries(&source).unwrap().unwrap();
    let forked = support::Ego::at(home.path(), &root, None)
        .subcommand("fork")
        .arg(&source.to_string())
        .args([
            "--at",
            &answer.to_string(),
            "--session-dir",
            sessions.path().to_str().unwrap(),
        ])
        .run();
    assert_eq!(
        forked.code,
        Some(0),
        "completed message fork failed: {}",
        forked.stderr
    );
    let child: SessionId = forked.stdout.trim().parse().unwrap();
    let replay = ReplayContext::new(&store, &store).replay(&child).unwrap();
    let messages = replay.conversation().messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(
        messages[0].content,
        vec![ContentBlock::Text {
            text: "first question".into()
        }]
    );
    assert_eq!(
        messages[1].content,
        vec![ContentBlock::Text {
            text: "first answer".into()
        }]
    );
    assert_eq!(store.entries(&source).unwrap().unwrap(), before);
    let state = SessionState::replay(
        store
            .entries(&child)
            .unwrap()
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry {
                SessionEntry::Event(event) => Some(event),
                _ => None,
            }),
    );
    assert!(state.unacknowledged_prompts().is_empty());
}
