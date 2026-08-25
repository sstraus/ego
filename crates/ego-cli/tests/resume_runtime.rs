//! RED contract for answer-bearing `ego resume`.
//!
//! Parsing is intentionally separate from application.  Invalid, duplicate,
//! extra, and already-settled answers must be rejected by the opened-session
//! replay gate before any provider or generation work; a valid batch is
//! applied once against the stored identities and then the same turn resumes.

use ego_cli::cli::{Invocation, parse};
use ego_core::event::id::ItemId;

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

// The process-level fixture is intentionally replaced by this narrow final
// seam: opening/replaying and validation must happen once before run_with
// starts materials, provider, preflight, or generation.  This remains RED
// until the CLI exposes the durable application operation.
#[allow(dead_code)]
fn apply_one_answer_batch(
    opened: &mut ego_cli::session::Opened<'_>,
    answers: &[ego_cli::cli::ResumeAnswer],
) {
    let _ = ego_cli::run::apply_resume_answers_once(opened, answers);
}
