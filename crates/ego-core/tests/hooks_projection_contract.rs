//! Hook events are internal evidence and never public payloads.

use ego_core::digest::sha256_hex;
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::hook::{
    HookActivation, HookCatalogueEntryEvidence, HookCatalogueEvidence, HookDecision,
    HookDescriptorEvidence, HookFeedback, HookInvocationId, HookLifecycle, HookOccurrenceKey,
    HookOccurrenceOwner, HookOutcome, HookPlan, HookPlanEntry, HookResult, HookSourceAuthority,
    HookSourceLocation,
};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::public::project_public;
use ego_core::store::evidence::{EvidenceId, EvidenceRef};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn descriptor() -> HookDescriptorEvidence {
    HookDescriptorEvidence::new(
        HookSourceAuthority::Project,
        HookSourceLocation::parse("hooks/project.json").unwrap(),
        DIGEST.parse().unwrap(),
        0,
    )
}

fn feedback(hook: ego_core::event::hook::HookId) -> HookFeedback {
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

fn event(seq: Seq, kind: EventKind, producer: Producer) -> DomainEvent {
    DomainEvent {
        env: Envelope {
            seq,
            id: EventId::new(),
            turn: TurnId::new(),
            item: Some(ItemId::new()),
            parent: None,
            ts: Timestamp::from_millis(1_000),
            producer,
        },
        kind,
    }
}

#[test]
fn all_four_hook_events_project_to_none_without_feedback_or_location_leaks() {
    let d = descriptor();
    let id = HookInvocationId::new();
    let mut seq = Seq::default();
    let events = [
        event(
            seq.next(),
            EventKind::HookCatalogueAdmitted {
                descriptors: HookCatalogueEvidence::new(vec![HookCatalogueEntryEvidence::new(
                    d.clone(),
                    HookActivation::Persistent,
                )])
                .unwrap(),
            },
            Producer::Main,
        ),
        event(
            seq.next(),
            EventKind::HookLifecycleReached {
                occurrence: HookOccurrenceKey::new(
                    HookLifecycle::PostToolUse,
                    HookOccurrenceOwner::Session {
                        session: SessionId::new(),
                    },
                ),
                subject: ego_core::event::hook::HookSubject::Session,
                plan: HookPlan::new(vec![HookPlanEntry::new(d.id(), HookActivation::Persistent)])
                    .unwrap(),
            },
            Producer::Main,
        ),
        event(
            seq.next(),
            EventKind::HookInvocationStarted {
                occurrence: EventId::new(),
                hook: d.id(),
                invocation: id,
                lifecycle: HookLifecycle::PostToolUse,
                subject: ego_core::event::hook::HookSubject::Session,
                activation: ego_core::event::hook::HookActivation::Persistent,
            },
            Producer::Main,
        ),
        event(
            seq.next(),
            EventKind::HookInvocationSettled {
                occurrence: EventId::new(),
                hook: d.id(),
                invocation: id,
                result: HookResult::Outcome {
                    outcome: HookOutcome::new(HookDecision::Block, Some(feedback(d.id()))),
                },
            },
            Producer::Hook(d.id()),
        ),
    ];
    for event in events {
        assert_eq!(
            project_public(&event),
            None,
            "hook evidence must stay private"
        );
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(encoded.contains("hook"));
        assert!(!encoded.contains("secret"));
    }
}

#[test]
fn hook_projection_accepts_no_public_shape_even_when_feedback_is_present() {
    let d = descriptor();
    let event = event(
        Seq::default(),
        EventKind::HookInvocationSettled {
            occurrence: EventId::new(),
            hook: d.id(),
            invocation: HookInvocationId::new(),
            result: HookResult::Outcome {
                outcome: HookOutcome::new(HookDecision::Block, Some(feedback(d.id()))),
            },
        },
        Producer::Hook(d.id()),
    );
    assert!(project_public(&event).is_none());
    let debug = format!("{event:?}");
    assert!(!debug.contains("unqualified_system"));
}
