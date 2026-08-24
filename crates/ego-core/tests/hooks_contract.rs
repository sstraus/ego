//! RED contract for the durable hook evidence schema (regression case).
//!
//! These assertions are about the final typed wire contract. Hook identity
//! and feedback provenance are audit data, not stringly-shaped test input.

use ego_core::digest::sha256_hex;
use ego_core::event::domain::EventKind;
use ego_core::event::hook::{
    HookCatalogueEvidence, HookContentDigest, HookDecision, HookDescriptorEvidence, HookFailure,
    HookFailureKind, HookFeedback, HookId, HookInvocationId, HookLifecycle, HookResult,
    HookSourceAuthority, HookSourceLocation,
};
use ego_core::event::id::{ItemId, Producer};
use ego_core::store::evidence::{EvidenceId, EvidenceRef};
use serde_json::json;

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const EVIDENCE_DIGEST: &str = "e256ee8e7aff6957a781d8328f0f68e26996564c81fa458da59fbca2305138ad";

fn digest() -> HookContentDigest {
    HookContentDigest::parse(DIGEST).unwrap()
}
fn location() -> HookSourceLocation {
    HookSourceLocation::parse("hooks/project.json").unwrap()
}
fn authority() -> HookSourceAuthority {
    HookSourceAuthority::Project
}
fn hook(ordinal: u32) -> HookDescriptorEvidence {
    HookDescriptorEvidence::new(authority(), location(), digest(), ordinal)
}
fn evidence_for(text: &str) -> EvidenceRef {
    EvidenceRef {
        id: EvidenceId::parse(sha256_hex(text.as_bytes())).unwrap(),
        bytes: text.len() as u64,
    }
}
fn feedback(hook: HookId, bytes: u64) -> HookFeedback {
    let reference = evidence_for("continue");
    assert_eq!(reference.bytes, bytes);
    HookFeedback::complete(hook, reference, "continue").unwrap()
}
fn eclair_evidence() -> EvidenceRef {
    evidence_for("éclair")
}

#[test]
fn hook_id_is_v1_framed_reproducible_and_strictly_encoded() {
    let a = HookId::derive(authority(), &location(), &digest(), 0);
    let b = HookId::derive(authority(), &location(), &digest(), 1);
    assert_ne!(a, b);
    assert_eq!(a, HookId::derive(authority(), &location(), &digest(), 0));
    assert_eq!(a.to_string().len(), 67);
    assert_eq!(
        a.to_string(),
        "h1:c0682a74a78c2a885d6fb8a4755fb5b85f6b1120b156678c590eb9bb8924270e"
    );
    assert!(HookId::parse(&a.to_string()).is_ok());
    for invalid in [
        "H1:deadbeef",
        "h1:DEADBEEF",
        "h1:00",
        "h2:0000000000000000000000000000000000000000000000000000000000000000",
    ] {
        assert!(HookId::parse(invalid).is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn identity_rejects_invalid_authority_relative_locations_digest_and_invocation_ids() {
    for value in [
        "",
        "/root",
        "a//b",
        "a/./b",
        "a/../b",
        "a\\b",
        "a/",
        "a\n",
        "C:/hooks/x",
    ] {
        assert!(
            HookSourceLocation::parse(value).is_err(),
            "accepted {value:?}"
        );
    }
    assert!(HookSourceLocation::parse(&"x".repeat(4097)).is_err());
    assert!(HookContentDigest::parse(&DIGEST.to_ascii_uppercase()).is_err());
    assert!(HookContentDigest::parse(&DIGEST[..63]).is_err());
    assert!(HookInvocationId::parse("00000000-0000-4000-8000-000000000000").is_err());
    assert!(HookInvocationId::parse("00000000-0000-0000-0000-000000000001").is_err());
    assert!(HookInvocationId::parse("not-an-id").is_err());
}

#[test]
fn descriptor_recomputes_identity_and_catalogue_rejects_duplicates_atomically() {
    let descriptor = hook(3);
    assert_eq!(
        descriptor.id(),
        HookId::derive(authority(), &location(), &digest(), 3)
    );
    assert!(HookCatalogueEvidence::new(vec![descriptor.clone(), descriptor]).is_err());
    assert!(HookCatalogueEvidence::new(vec![hook(0), hook(0)]).is_err());
    assert!(HookCatalogueEvidence::new(vec![hook(0), hook(1)]).is_ok());
    assert!(
        HookCatalogueEvidence::new(Vec::new())
            .unwrap()
            .as_slice()
            .is_empty()
    );
    let mut mismatched_id = serde_json::to_value(hook(0)).unwrap();
    mismatched_id["id"] = json!(hook(1).id().to_string());
    assert!(serde_json::from_value::<HookDescriptorEvidence>(mismatched_id).is_err());
    assert!(
        serde_json::from_value::<HookDescriptorEvidence>(json!({
            "id": "h1:00", "source": "project", "location": "hooks/x", "digest": DIGEST,
            "ordinal": 0, "unknown": true
        }))
        .is_err()
    );
}

#[test]
fn feedback_is_typed_source_bound_and_truthful_at_utf8_and_byte_bounds() {
    let id = hook(0).id();
    let complete = feedback(id, 8);
    assert_eq!(complete.hook(), id);
    assert_eq!(complete.omitted_bytes(), 0);
    assert_eq!(complete.tail(), None);
    assert_eq!(complete.reference(), &evidence_for("continue"));

    let oversized = "é".repeat(4096);
    let truncated =
        HookFeedback::truncated(id, evidence_for(&oversized), &oversized, 2048, 2048, 4096)
            .unwrap();
    assert_eq!(truncated.head_bytes() + truncated.tail_bytes(), 4096);
    assert_eq!(truncated.omitted_bytes(), 4096);
    let nul = HookFeedback::complete(id, evidence_for("x\0"), "x\0").unwrap();
    assert_eq!(nul.head(), "x\0");

    for bad in [
        json!({"hook":"h1:00","full":format!("{EVIDENCE_DIGEST}:1"),"head":"x","tail":null,"omitted_bytes":0}),
        json!({"hook":id.to_string(),"full":format!("{EVIDENCE_DIGEST}:1"),"head":"x","tail":"y","omitted_bytes":999}),
    ] {
        assert!(serde_json::from_value::<HookFeedback>(bad).is_err());
    }
}

#[test]
fn feedback_mutations_fail_digest_length_truncation_and_source_validation() {
    let id = hook(0).id();
    let good = serde_json::to_value(feedback(id, 8)).unwrap();
    let mutations = [
        ("full", json!(format!("{EVIDENCE_DIGEST}:9"))),
        ("omitted_bytes", json!(1)),
        ("head", json!("changed")),
    ];
    for (field, value) in mutations {
        let mut candidate = good.clone();
        candidate[field] = value;
        assert!(
            serde_json::from_value::<HookFeedback>(candidate).is_err(),
            "mutation {field} accepted"
        );
    }
    let mut other_source = good;
    other_source["hook"] = json!(hook(1).id().to_string());
    assert!(
        serde_json::from_value::<HookFeedback>(other_source).is_ok(),
        "source tag is valid evidence until reducer settlement"
    );
}

#[test]
fn tagged_results_require_typed_decisions_feedback_and_failure_kinds() {
    let id = hook(0).id();
    assert!(HookFeedback::complete(id, eclair_evidence(), "éclair").is_ok());
    assert!(HookDecision::Continue.is_continue());
    assert!(HookDecision::Block.is_blocking());
    assert!(HookDecision::RequestEscalation.is_escalation());
    let _ = ego_core::event::hook::HookOutcome::new(HookDecision::Continue, None);
    let _ = ego_core::event::hook::HookOutcome::new(HookDecision::Continue, Some(feedback(id, 8)));
    let _ = ego_core::event::hook::HookOutcome::new(HookDecision::Block, Some(feedback(id, 8)));
    let _ = ego_core::event::hook::HookOutcome::new(
        HookDecision::RequestEscalation,
        Some(feedback(id, 8)),
    );
    let _ = ego_core::event::hook::HookOutcome::new(
        HookDecision::Block,
        Some(feedback(hook(1).id(), 8)),
    );
    for kind in [
        HookFailureKind::EffectDenied,
        HookFailureKind::TimedOut,
        HookFailureKind::Cancelled,
        HookFailureKind::MalformedResult,
        HookFailureKind::BackendUnavailable,
        HookFailureKind::HandlerFailed,
        HookFailureKind::EvidenceFailed,
        HookFailureKind::DeliveryUncertain,
    ] {
        let _ = HookFailure::new(kind, Some(feedback(id, 8)));
    }
    assert!(
        serde_json::from_value::<HookResult>(
            json!({"Outcome":{"outcome":{"decision":"continue"}}})
        )
        .is_err()
    );
}

#[test]
fn events_are_exactly_the_three_typed_hook_facts_and_no_legacy_decoder_exists() {
    let descriptor = hook(0);
    let invocation = HookInvocationId::new();
    let subject = ego_core::event::hook::HookSubject::Item {
        item: ItemId::new(),
    };
    let _ = EventKind::HookCatalogueAdmitted {
        descriptors: HookCatalogueEvidence::new(vec![descriptor.clone()]).unwrap(),
    };
    let _ = EventKind::HookInvocationStarted {
        hook: descriptor.id(),
        invocation,
        lifecycle: HookLifecycle::PreToolUse,
        subject,
    };
    let _ = EventKind::HookInvocationSettled {
        hook: descriptor.id(),
        invocation,
        result: HookResult::Outcome {
            outcome: ego_core::event::hook::HookOutcome::new(HookDecision::Continue, None),
        },
    };
    let old = json!({"kind":"HookFired","name":"old","blocked":false});
    assert!(serde_json::from_value::<EventKind>(old).is_err());
}

#[test]
fn lifecycle_phase_is_derived_and_hook_producers_cannot_recurse() {
    assert!(HookLifecycle::PreToolUse.phase().is_pre_effect());
    assert!(HookLifecycle::PostToolUse.phase().is_observer());
    assert!(!Producer::Hook(hook(0).id()).may_trigger_hooks());
}
