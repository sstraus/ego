//! contract for answer-bearing `ego resume`.
//!
//! Parsing is intentionally separate from application. Invalid, duplicate,
//! extra, and already-settled answers must be rejected by the opened-session
//! replay gate before any provider or generation work; a valid batch is
//! applied once against the stored identities and then the same turn resumes.

use std::process::Command;
use std::str::FromStr;

use ego_cli::cli::{Invocation, parse};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::ItemId;
use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::question::{QuestionMode, QuestionOption, QuestionRequest, QuestionTarget};
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{JsonlStore, OsIo};

mod support;

fn id() -> String {
    ItemId::new().to_string()
}

#[test]
fn resume_accepts_repeatable_json_answers_without_reinterpreting_the_grammar() {
    let first = id();
    let second = id();
    let invocation = parse(vec![
        "ego".into(),
        "resume".into(),
        "--answer".into(),
        format!(r#"{first}="yes""#),
        "--answer".into(),
        format!(r#"{second}=["a","b"]"#),
    ])
    .expect("repeatable --answer parses");

    let Invocation::Resume(args) = invocation else {
        panic!("resume must retain the answer-bearing invocation");
    };
    assert_eq!(args.answers.len(), 2);
    assert_eq!(args.answers[0].id.to_string(), first);
    assert_eq!(args.answers[0].json, r#""yes""#);
    assert_eq!(args.answers[1].json, r#"["a","b"]"#);
}

#[test]
fn malformed_json_is_rejected_before_an_opened_session_can_be_mutated() {
    let question = id();
    let error = parse(vec![
        "ego".into(),
        "resume".into(),
        "--answer".into(),
        format!(r#"{question}=not-json"#),
    ])
    .expect_err("answers are JSON, not an opaque string");
    assert!(error.to_string().contains("valid JSON"));
}

fn append_model_question(
    home: &std::path::Path,
    workspace: &std::path::Path,
    id: SessionId,
) -> ItemId {
    let sessions = home.join("sessions");
    let workspace = std::fs::canonicalize(workspace).expect("canonical workspace");
    let store = JsonlStore::new(sessions, workspace, Box::new(OsIo));
    let question = ItemId::new();
    let turn = TurnId::new();
    let event = DomainEvent {
        env: Envelope {
            seq: Seq::default(),
            id: EventId::new(),
            turn,
            item: Some(question),
            parent: None,
            ts: Timestamp::from_millis(1_000),
            producer: Producer::Main,
        },
        kind: EventKind::QuestionRequested {
            request: QuestionRequest::new(
                "Choose a target",
                QuestionMode::Single,
                vec![QuestionOption::new("staging", "Staging")],
                false,
            ),
            target: QuestionTarget::ModelTool {
                tool_call: ItemId::new(),
            },
        },
    };
    store
        .append(&id, &event)
        .expect("foreign question is durable");
    question
}

#[test]
fn direct_ci_model_answer_refuses_before_question_or_tool_mutation_when_provider_is_unready() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = home.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("session root");
    let artifacts = home.path().join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifact root");
    std::fs::write(
        workspace.path().join("instructions.md"),
        "resume instructions\n",
    )
    .expect("instructions fixture");
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[profile.ci]\nmodel = \"ollama/qwen3\"\nsandbox = \"off\"\nsession_dir = {:?}\nartifact_dir = {:?}\n",
            sessions,
            artifacts,
        ),
    )
    .expect("model fixture");
    let session =
        SessionId::from_str("0198c7f4-9c1f-7a2b-8c3d-4e5f60718293").expect("fixture session id");
    let question = append_model_question(home.path(), workspace.path(), session);
    let answer = format!("{}=\"staging\"", question);
    let output = Command::new(env!("CARGO_BIN_EXE_ego"))
        .env_clear()
        .env("HOME", home.path())
        .env("EGO_HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .env("OLLAMA_HOST", "127.0.0.1:1")
        .current_dir(workspace.path())
        .args([
            "resume",
            session.to_string().as_str(),
            "resume",
            "--profile",
            "ci",
            "--instructions",
            workspace
                .path()
                .join("instructions.md")
                .to_str()
                .expect("instructions path"),
            "--session-dir",
            sessions.to_str().expect("session path"),
            "--artifact-dir",
            artifacts.to_str().expect("artifact path"),
            "--answer",
            &answer,
        ])
        .output()
        .expect("ego resume process");
    assert!(!output.status.success(), "resume unexpectedly succeeded");
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("did not answer with what was needed"),
        "provider readiness diagnostic missing: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = sessions.join(session.to_string()).join("events.jsonl");
    let events = std::fs::read_to_string(log).expect("session events");
    assert!(
        !events.contains("question_settled"),
        "question was settled before readiness"
    );
    assert!(
        !events.contains("tool_call_completed"),
        "tool completion was persisted before readiness"
    );
}
