//! The fixture was captured from the version-2 writer at fa3b66a7 on rb.
//! Command and output: ~/Gits/.tmp/example-evidence-b.log.

use ego_core::event::time::{Clock, Timestamp};
use ego_core::suspension::{ContinuationJournal, ContinuationState};
use ego_tools::mcp_continuation::JsonlContinuationJournal;
use ego_tools::store::OsIo;
use serde_json::json;

struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(1000)
    }
}

#[test]
fn recorded_v2_single_form_round_preserves_every_authority_byte_and_answer_meaning() {
    // Catches: upgrading a pending record changes its grant, deadline, or accepted answer shape.
    let root = tempfile::tempdir().unwrap();
    let fixture = include_bytes!("fixtures/mcp/continuation-v2.json");
    std::fs::write(root.path().join("continuations.jsonl"), fixture).unwrap();
    let before: serde_json::Value = serde_json::from_slice(fixture).unwrap();
    let journal = JsonlContinuationJournal::new(root.path(), Box::new(OsIo), Box::new(FixedClock));
    let record = ContinuationJournal::load(&journal, "continuation-1").unwrap();
    assert_eq!(serde_json::to_value(&record).unwrap(), before["record"]);
    let round = record.question_round().unwrap();
    round.validate().unwrap();
    assert_eq!(round.requests.len(), 1);
    assert!(round.requests.contains_key("request-1"));
    assert_eq!(record.expires_at(), Timestamp::from_millis(301000));
    ContinuationJournal::accept_answer(
        &journal,
        "continuation-1",
        br#"{"answer":"yes"}"#,
        Timestamp::from_millis(1000),
    )
    .unwrap();
    let reopened = JsonlContinuationJournal::new(root.path(), Box::new(OsIo), Box::new(FixedClock));
    let accepted = ContinuationJournal::load(&reopened, "continuation-1").unwrap();
    let mut expected = before["record"].clone();
    expected["state"] = json!("accepted");
    expected["accepted_answer"] = json!(br#"{"answer":"yes"}"#.to_vec());
    assert_eq!(serde_json::to_value(&accepted).unwrap(), expected);
    let lines = std::fs::read_to_string(root.path().join("continuations.jsonl")).unwrap();
    let last: serde_json::Value = serde_json::from_str(lines.lines().last().unwrap()).unwrap();
    assert_eq!(last["version"], json!(3));
    assert_eq!(last["record"]["grant"], before["record"]["grant"]);
    let _claim = reopened.claim("continuation-1").unwrap();
    let after_claim =
        JsonlContinuationJournal::new(root.path(), Box::new(OsIo), Box::new(FixedClock));
    assert_eq!(
        after_claim.state("continuation-1").unwrap(),
        ContinuationState::Claimed
    );
    assert!(after_claim.claim("continuation-1").is_err());
}
