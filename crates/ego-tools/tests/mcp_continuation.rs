//! Coarse RED contract for durable MCP continuation authority.

use ego_core::event::time::Timestamp;
use ego_core::suspension::{ContinuationRecord, ContinuationState};
use ego_core::suspension::{ContinuationSubject, ResumeGrant};
use ego_tools::mcp_continuation::JsonlContinuationJournal;
use std::time::{SystemTime, UNIX_EPOCH};

fn record() -> ContinuationRecord {
    let now_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    record_until(Timestamp::from_millis(now_millis.saturating_add(300_000)))
}

fn record_until(expires_at: Timestamp) -> ContinuationRecord {
    let subject = ContinuationSubject::new("alpha", "tools/call", "capture-7", "tool-fingerprint")
        .expect("continuation subject");
    let grant = ResumeGrant::new("prepared-call-fingerprint", br#"sealed-request-state"#)
        .expect("sealed resume grant");
    ContinuationRecord::new("continuation-1", subject, grant, expires_at)
        .expect("validated continuation")
}

#[test]
fn accepted_answer_is_durable_before_single_use_claim() {
    let root = tempfile::tempdir().expect("continuation directory");
    let journal = JsonlContinuationJournal::open(root.path()).expect("journal");
    let id = journal
        .accept(record(), br#"{"inputResponses":[{"answer":"yes"}]}"#)
        .expect("accepted answer");
    assert_eq!(
        journal.state(&id).expect("state"),
        ContinuationState::Accepted
    );
    assert!(journal.claim(&id).expect("claim").is_first());
    assert!(journal.claim(&id).expect("duplicate claim").is_duplicate());
}

#[test]
fn crash_after_claim_is_uncertain_and_never_replayed_automatically() {
    let root = tempfile::tempdir().expect("continuation directory");
    let journal = JsonlContinuationJournal::open(root.path()).expect("journal");
    let id = journal
        .accept(record(), br#"{"inputResponses":[{"answer":"yes"}]}"#)
        .expect("accepted answer");
    journal.claim(&id).expect("claim");
    journal.mark_crashed_after_claim(&id).expect("crash marker");
    let restarted = JsonlContinuationJournal::open(root.path()).expect("restart");
    assert_eq!(
        restarted.state(&id).expect("state"),
        ContinuationState::Uncertain
    );
    assert!(restarted.replay(&id).expect("replay").is_none());
}

#[test]
fn restart_rejects_changed_capture_tool_schema_or_prepared_call_fingerprint() {
    let root = tempfile::tempdir().expect("continuation directory");
    let journal = JsonlContinuationJournal::open(root.path()).expect("journal");
    let id = journal
        .accept(record(), br#"{"inputResponses":[{"answer":"yes"}]}"#)
        .expect("accepted answer");
    assert!(
        journal
            .resume_with_fingerprint(
                &id,
                "capture-other",
                "tool-fingerprint",
                "prepared-call-fingerprint"
            )
            .is_err()
    );
    assert!(
        journal
            .resume_with_fingerprint(
                &id,
                "capture-7",
                "schema-other",
                "prepared-call-fingerprint"
            )
            .is_err()
    );
    assert!(
        journal
            .resume_with_fingerprint(&id, "capture-7", "tool-fingerprint", "prepared-call-other")
            .is_err()
    );
}

#[test]
fn expired_wall_clock_continuation_is_rejected_before_claim() {
    let root = tempfile::tempdir().expect("continuation directory");
    let journal = JsonlContinuationJournal::open(root.path()).expect("journal");
    let expired = record_until(Timestamp::from_millis(0));
    let id = journal
        .accept(expired, br#"{"inputResponses":[{"answer":"yes"}]}"#)
        .expect("durable expired answer");
    assert!(
        journal
            .claim(&id)
            .expect_err("expired claim must fail")
            .is_expired()
    );
}
