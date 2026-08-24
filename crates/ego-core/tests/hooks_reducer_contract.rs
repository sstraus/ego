//! RED reducer contract for hook catalogue and invocation evidence.

use ego_core::digest::sha256_hex;
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::hook::{
    HookCatalogueEvidence, HookDecision, HookDescriptorEvidence, HookFailure, HookFailureKind,
    HookFeedback, HookId, HookInvocationId, HookLifecycle, HookOutcome, HookResult,
    HookSourceAuthority, HookSourceLocation,
};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::state::{HookReplayDisposition, RefusalReason, SessionState};
use ego_core::store::evidence::{EvidenceId, EvidenceRef};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn descriptor(authority: HookSourceAuthority, ordinal: u32) -> HookDescriptorEvidence {
    HookDescriptorEvidence::new(
        authority,
        HookSourceLocation::parse("hooks/project.json").unwrap(),
        DIGEST.parse().unwrap(),
        ordinal,
    )
}

fn subject(item: ItemId) -> ego_core::event::hook::HookSubject {
    ego_core::event::hook::HookSubject::Item { item }
}

struct Fixture {
    turn: TurnId,
    item: ItemId,
    seq: Seq,
    parent: Option<EventId>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            turn: TurnId::new(),
            item: ItemId::new(),
            seq: Seq::default(),
            parent: None,
        }
    }

    fn event(&mut self, kind: EventKind, producer: Producer) -> DomainEvent {
        let event = DomainEvent {
            env: Envelope {
                seq: self.seq.next(),
                id: EventId::new(),
                turn: self.turn,
                item: Some(self.item),
                parent: self.parent,
                ts: Timestamp::from_millis(1_000),
                producer,
            },
            kind,
        };
        self.parent = Some(event.env.id);
        event
    }

    fn admission(&mut self, descriptors: Vec<HookDescriptorEvidence>) -> DomainEvent {
        self.event(
            EventKind::HookCatalogueAdmitted {
                descriptors: HookCatalogueEvidence::new(descriptors).unwrap(),
            },
            Producer::Main,
        )
    }

    fn start(
        &mut self,
        hook: HookId,
        invocation: HookInvocationId,
        lifecycle: HookLifecycle,
    ) -> DomainEvent {
        self.event(
            EventKind::HookInvocationStarted {
                hook,
                invocation,
                lifecycle,
                subject: subject(self.item),
            },
            Producer::Main,
        )
    }

    fn settle(
        &mut self,
        hook: HookId,
        invocation: HookInvocationId,
        result: HookResult,
    ) -> DomainEvent {
        self.event(
            EventKind::HookInvocationSettled {
                hook,
                invocation,
                result,
            },
            Producer::Hook(hook),
        )
    }
}

fn continue_result() -> HookResult {
    HookResult::Outcome {
        outcome: HookOutcome::new(HookDecision::Continue, None),
    }
}

fn feedback(hook: HookId) -> HookFeedback {
    HookFeedback::complete(
        hook,
        EvidenceRef {
            id: EvidenceId::parse(sha256_hex(b"continue")).unwrap(),
            bytes: 8,
        },
        "continue",
    )
    .unwrap()
}

fn assert_reason(state: &SessionState, event: &DomainEvent, reason: RefusalReason) {
    let refusal = state.refusals.last().expect("invalid event is recorded");
    assert_eq!(refusal.event, event.env.id);
    assert_eq!(refusal.reason, reason);
}

#[test]
fn reducer_is_total_and_accepts_admission_start_settlement_in_order() {
    let mut f = Fixture::new();
    let d = descriptor(HookSourceAuthority::Project, 0);
    let invocation = HookInvocationId::new();
    let mut state = SessionState::default();
    state.apply(&f.admission(vec![d.clone()]));
    state.apply(&f.start(d.id(), invocation, HookLifecycle::PreToolUse));
    state.apply(&f.settle(d.id(), invocation, continue_result()));
    assert!(state.refusals.is_empty());
    assert!(state.live_hooks().is_empty());
    assert_eq!(state.hook_settlements().len(), 1);
    assert_eq!(state.hook_catalogue().as_slice(), &[d]);
}

#[test]
fn reducer_records_refusals_without_result_errors_or_state_effects() {
    let mut f = Fixture::new();
    let d = descriptor(HookSourceAuthority::Project, 0);
    let unknown = HookId::derive(
        HookSourceAuthority::User,
        &HookSourceLocation::parse("hooks/user.json").unwrap(),
        &DIGEST.parse().unwrap(),
        0,
    );
    let invocation = HookInvocationId::new();
    let mut state = SessionState::default();
    let unknown_start = f.start(unknown, invocation, HookLifecycle::PreToolUse);
    state.apply(&unknown_start);
    assert_reason(&state, &unknown_start, RefusalReason::HookNotAdmitted);
    state.apply(&f.admission(vec![d.clone()]));
    let before_start = f.settle(d.id(), invocation, continue_result());
    state.apply(&before_start);
    assert_reason(&state, &before_start, RefusalReason::HookInvocationUnknown);
    let accepted_start = f.start(d.id(), invocation, HookLifecycle::PreToolUse);
    let accepted_start_id = accepted_start.env.id;
    state.apply(&accepted_start);
    let duplicate_start = f.start(d.id(), invocation, HookLifecycle::PreToolUse);
    state.apply(&duplicate_start);
    assert_reason(
        &state,
        &duplicate_start,
        RefusalReason::HookInvocationAlreadyStarted,
    );
    f.parent = Some(accepted_start_id);
    let settlement = f.settle(d.id(), invocation, continue_result());
    state.apply(&settlement);
    let duplicate_settlement = f.settle(d.id(), invocation, continue_result());
    state.apply(&duplicate_settlement);
    assert_reason(
        &state,
        &duplicate_settlement,
        RefusalReason::HookInvocationAlreadySettled,
    );
    assert!(state.live_hooks().is_empty());
    assert_eq!(state.hook_settlements().len(), 1);
}

#[test]
fn reducer_covers_catalogue_replacement_live_survival_and_empty_clear() {
    let mut f = Fixture::new();
    let old = descriptor(HookSourceAuthority::Project, 0);
    let replacement = descriptor(HookSourceAuthority::Project, 1);
    let invocation = HookInvocationId::new();
    let mut state = SessionState::default();
    state.apply(&f.admission(vec![old.clone()]));
    let start = f.start(old.id(), invocation, HookLifecycle::PostToolUse);
    let start_id = start.env.id;
    state.apply(&start);
    state.apply(&f.admission(vec![replacement.clone()]));
    assert_eq!(state.hook_catalogue().as_slice(), &[replacement]);
    assert_eq!(
        state.live_hooks().len(),
        1,
        "replacement cannot invalidate live evidence"
    );
    f.parent = Some(start_id);
    state.apply(&f.settle(old.id(), invocation, continue_result()));
    assert!(state.live_hooks().is_empty());
    state.apply(&f.admission(Vec::new()));
    assert!(state.hook_catalogue().as_slice().is_empty());
}

#[test]
fn reducer_refuses_settlement_hook_invocation_envelope_parent_and_producer_mismatches() {
    let mut f = Fixture::new();
    let d = descriptor(HookSourceAuthority::Project, 0);
    let other = descriptor(HookSourceAuthority::Project, 1);
    let invocation = HookInvocationId::new();
    let other_invocation = HookInvocationId::new();
    let mut state = SessionState::default();
    state.apply(&f.admission(vec![d.clone(), other.clone()]));
    let start = f.start(d.id(), invocation, HookLifecycle::PreToolUse);
    let start_id = start.env.id;
    state.apply(&start);
    f.parent = Some(start_id);
    let wrong_invocation = f.settle(d.id(), other_invocation, continue_result());
    state.apply(&wrong_invocation);
    assert_reason(
        &state,
        &wrong_invocation,
        RefusalReason::HookInvocationUnknown,
    );
    f.parent = Some(start_id);
    let wrong_hook = f.settle(other.id(), invocation, continue_result());
    state.apply(&wrong_hook);
    assert_reason(
        &state,
        &wrong_hook,
        RefusalReason::HookInvocationHookMismatch,
    );
    f.parent = Some(start_id);
    let wrong_feedback = f.settle(
        d.id(),
        invocation,
        HookResult::Outcome {
            outcome: HookOutcome::new(HookDecision::Continue, Some(feedback(other.id()))),
        },
    );
    state.apply(&wrong_feedback);
    assert_reason(
        &state,
        &wrong_feedback,
        RefusalReason::HookFeedbackSourceMismatch,
    );
    f.parent = Some(start_id);
    let mut wrong_item = f.settle(d.id(), invocation, continue_result());
    wrong_item.env.item = Some(ItemId::new());
    state.apply(&wrong_item);
    assert_reason(
        &state,
        &wrong_item,
        RefusalReason::HookInvocationItemMismatch,
    );
    f.parent = Some(start_id);
    let mut wrong_parent = f.settle(d.id(), invocation, continue_result());
    wrong_parent.env.parent = None;
    state.apply(&wrong_parent);
    assert_reason(
        &state,
        &wrong_parent,
        RefusalReason::HookInvocationParentMismatch,
    );
    f.parent = Some(start_id);
    let mut wrong_turn = f.settle(d.id(), invocation, continue_result());
    wrong_turn.env.turn = TurnId::new();
    state.apply(&wrong_turn);
    assert_reason(
        &state,
        &wrong_turn,
        RefusalReason::HookInvocationTurnMismatch,
    );
    f.parent = Some(start_id);
    let mut wrong_producer = f.settle(d.id(), invocation, continue_result());
    wrong_producer.env.producer = Producer::Main;
    state.apply(&wrong_producer);
    assert_reason(&state, &wrong_producer, RefusalReason::HookProducerMismatch);
    assert_eq!(
        state.live_hooks().len(),
        1,
        "mismatches cannot settle the live invocation"
    );
    assert_eq!(state.hook_settlements().len(), 0);
    assert_eq!(state.refusals.len(), 7);
    f.parent = Some(start_id);
    state.apply(&f.settle(d.id(), invocation, continue_result()));
    assert!(state.live_hooks().is_empty());
    assert_eq!(state.hook_settlements().len(), 1);
}

#[test]
fn reducer_refuses_start_subject_envelope_and_hook_producer_recursion() {
    let mut f = Fixture::new();
    let d = descriptor(HookSourceAuthority::Project, 0);
    let mut state = SessionState::default();
    state.apply(&f.admission(vec![d.clone()]));

    let mut wrong_session = f.start(d.id(), HookInvocationId::new(), HookLifecycle::PreToolUse);
    wrong_session.kind = EventKind::HookInvocationStarted {
        hook: d.id(),
        invocation: HookInvocationId::new(),
        lifecycle: HookLifecycle::PreToolUse,
        subject: ego_core::event::hook::HookSubject::Session,
    };
    state.apply(&wrong_session);
    assert_reason(&state, &wrong_session, RefusalReason::HookSubjectMismatch);
    let mut wrong_turn = f.start(d.id(), HookInvocationId::new(), HookLifecycle::PreToolUse);
    wrong_turn.kind = EventKind::HookInvocationStarted {
        hook: d.id(),
        invocation: HookInvocationId::new(),
        lifecycle: HookLifecycle::PreToolUse,
        subject: ego_core::event::hook::HookSubject::Turn {
            turn: TurnId::new(),
        },
    };
    wrong_turn.env.item = None;
    state.apply(&wrong_turn);
    assert_reason(&state, &wrong_turn, RefusalReason::HookSubjectMismatch);
    let mut turn_with_item = f.start(d.id(), HookInvocationId::new(), HookLifecycle::PreToolUse);
    turn_with_item.kind = EventKind::HookInvocationStarted {
        hook: d.id(),
        invocation: HookInvocationId::new(),
        lifecycle: HookLifecycle::PreToolUse,
        subject: ego_core::event::hook::HookSubject::Turn { turn: f.turn },
    };
    turn_with_item.env.item = Some(ItemId::new());
    state.apply(&turn_with_item);
    assert_reason(&state, &turn_with_item, RefusalReason::HookSubjectMismatch);
    let mut wrong_item = f.start(d.id(), HookInvocationId::new(), HookLifecycle::PreToolUse);
    wrong_item.env.item = Some(ItemId::new());
    wrong_item.kind = EventKind::HookInvocationStarted {
        hook: d.id(),
        invocation: HookInvocationId::new(),
        lifecycle: HookLifecycle::PreToolUse,
        subject: ego_core::event::hook::HookSubject::Item { item: f.item },
    };
    state.apply(&wrong_item);
    assert_reason(&state, &wrong_item, RefusalReason::HookSubjectMismatch);
    let mut recursive = f.start(d.id(), HookInvocationId::new(), HookLifecycle::PreToolUse);
    recursive.env.producer = Producer::Hook(d.id());
    state.apply(&recursive);
    assert_reason(&state, &recursive, RefusalReason::RecursiveHookInvocation);
    assert!(state.live_hooks().is_empty());
    assert_eq!(state.refusals.len(), 5, "five invalid starts");
}

#[test]
fn unresolved_pre_effect_and_observer_replay_have_distinct_fail_closed_dispositions() {
    let d = descriptor(HookSourceAuthority::Project, 0);
    let pre = HookInvocationId::new();
    let observer = HookInvocationId::new();
    let mut f = Fixture::new();
    let mut state = SessionState::default();
    state.apply(&f.admission(vec![d.clone()]));
    let pre_start = f.start(d.id(), pre, HookLifecycle::PreToolUse);
    state.apply(&pre_start);
    let observer_start = f.start(d.id(), observer, HookLifecycle::PostToolUse);
    state.apply(&observer_start);
    let pre_live = state.live_hooks().get(&pre).expect("pre start is live");
    let observer_live = state
        .live_hooks()
        .get(&observer)
        .expect("observer start is live");
    assert_eq!(
        pre_live.replay_disposition(),
        HookReplayDisposition::FailClosed
    );
    assert_eq!(
        observer_live.replay_disposition(),
        HookReplayDisposition::ObserverUncertain
    );
    assert_eq!(pre_live.delivery(), DeliveryCertainty::PossiblySent);
    assert_eq!(observer_live.delivery(), DeliveryCertainty::PossiblySent);
    assert!(!pre_live.delivery().retry_is_safe());
    assert!(!observer_live.delivery().retry_is_safe());
}

#[test]
fn failure_settlement_is_typed_and_feedback_continuation_does_not_recurse() {
    let mut f = Fixture::new();
    let d = descriptor(HookSourceAuthority::Project, 0);
    let invocation = HookInvocationId::new();
    let mut state = SessionState::default();
    state.apply(&f.admission(vec![d.clone()]));
    state.apply(&f.start(d.id(), invocation, HookLifecycle::PreToolUse));
    state.apply(&f.settle(
        d.id(),
        invocation,
        HookResult::Failure {
            failure: HookFailure::new(HookFailureKind::TimedOut, None),
        },
    ));
    assert!(state.live_hooks().is_empty());
    assert_eq!(state.hook_settlements().len(), 1);
    assert!(!Producer::Hook(d.id()).may_trigger_hooks());
}

#[test]
fn reducer_refuses_observer_block_and_non_pre_escalation_but_accepts_continue() {
    let mut f = Fixture::new();
    let d = descriptor(HookSourceAuthority::Project, 0);
    let mut state = SessionState::default();
    state.apply(&f.admission(vec![d.clone()]));

    let observer = HookInvocationId::new();
    let start = f.start(d.id(), observer, HookLifecycle::PostToolUse);
    let start_id = start.env.id;
    state.apply(&start);
    f.parent = Some(start_id);
    let observer_block = f.settle(
        d.id(),
        observer,
        HookResult::Outcome {
            outcome: HookOutcome::new(HookDecision::Block, None),
        },
    );
    state.apply(&observer_block);
    assert_reason(&state, &observer_block, RefusalReason::HookBlockAtObserver);
    f.parent = Some(start_id);
    state.apply(&f.settle(d.id(), observer, continue_result()));

    let escalation = HookInvocationId::new();
    let escalation_start = f.start(d.id(), escalation, HookLifecycle::PostToolUse);
    let escalation_id = escalation_start.env.id;
    state.apply(&escalation_start);
    f.parent = Some(escalation_id);
    let escalation_result = f.settle(
        d.id(),
        escalation,
        HookResult::Outcome {
            outcome: HookOutcome::new(HookDecision::RequestEscalation, None),
        },
    );
    state.apply(&escalation_result);
    assert_reason(
        &state,
        &escalation_result,
        RefusalReason::HookEscalationOutsidePreToolUse,
    );
    f.parent = Some(escalation_id);
    state.apply(&f.settle(d.id(), escalation, continue_result()));
    assert_eq!(state.refusals.len(), 2);
    assert!(state.live_hooks().is_empty());
    assert_eq!(state.hook_settlements().len(), 2);
}
