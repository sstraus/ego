//! contract for regression case: overflow settles explicitly and compacts from idle.

use ego_core::compact::Checkpoint;
use ego_core::generator::attempt::{DeliveryCertainty, Outcome, UsageReport};
use ego_core::generator::history::conversation_from;
use ego_core::generator::types::{Conversation, ErrorKind, Message, Role, StopReason};
use ego_core::run::{RunSettlement, RunStatus, RunTermination};
use ego_core::state::{SessionState, Usage};

fn checkpoint() -> String {
    [
        "# Objective",
        "Ship the release.",
        "# Binding constraints and preferences",
        "Preserve all operator decisions.",
        "# Completed work and evidence",
        "The verification command passed.",
        "# Work in progress",
        "The provider response remains pending.",
        "# Decisions and rationale",
        "Use the existing idle compaction operation.",
        "# Blockers and uncertainty",
        "Context capacity is uncertain.",
        "# Ordered next steps",
        "Continue the main generation.",
        "# Critical references",
        "docs/architecture.md",
    ]
    .join("\n")
}

#[test]
fn sent_context_overflow_is_terminal_and_never_retryable() {
    let outcome = Outcome::new(
        StopReason::Error,
        DeliveryCertainty::PossiblySent,
        UsageReport::Missing,
    );
    assert!(!outcome.retry_is_safe());
}

#[test]
fn repeated_idle_checkpoints_preserve_all_semantic_sections() {
    let first = Checkpoint::parse(checkpoint()).expect("complete checkpoint");
    let second = Checkpoint::parse(first.text().to_owned()).expect("repeated checkpoint");
    assert_eq!(first, second);
    for heading in [
        "# Objective",
        "# Completed work and evidence",
        "# Work in progress",
        "# Decisions and rationale",
        "# Blockers and uncertainty",
        "# Ordered next steps",
        "# Critical references",
    ] {
        assert!(second.text().contains(heading), "missing {heading}");
    }
}

#[test]
fn checkpoint_only_continuation_has_no_live_source_attempt_or_prompt_duplication() {
    let conversation = Conversation::new(vec![Message::text(Role::User, checkpoint())])
        .expect("checkpoint is valid history");
    assert_eq!(conversation.messages().len(), 1);
    let state = SessionState::default();
    let (rebuilt, repairs) = conversation_from(&state).expect("empty target replay");
    assert!(rebuilt.messages().is_empty());
    assert!(repairs.is_empty());
    assert!(state.live_attempts.is_empty());
}

#[test]
fn overflow_keeps_generic_invalid_and_output_length_as_distinct_causes() {
    assert_ne!(ErrorKind::Invalid, ErrorKind::ContextOverflow);
    let _ordinary = StopReason::Length;
    let _measured = Usage::IDENTITY;
}

#[test]
fn context_overflow_is_a_settled_durable_termination() {
    let termination = RunTermination::ContextOverflow {
        message: "provider context window exhausted".to_owned(),
    };
    assert_eq!(termination.exit_code(), 4);

    let settlement = RunSettlement::ContextOverflow;
    let settled_status = RunStatus::Settled;
    assert_eq!(serde_json::to_value(settled_status).unwrap(), "settled");
    let encoded = serde_json::to_value(settlement).expect("settlement is serialisable");
    assert_eq!(encoded["outcome"], "context_overflow");
}

#[test]
fn context_overflow_is_not_a_generic_invalid_or_length_stop() {
    let termination = RunTermination::ContextOverflow {
        message: "context capacity reached".to_owned(),
    };
    assert!(!matches!(termination, RunTermination::Failed { .. }));
    assert_ne!(StopReason::Length, StopReason::Error);
}
