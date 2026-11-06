//! Contract for Windows unconfined-start acknowledgement.
//!
//! The acknowledgement is a host-risk fact, not project trust and not user
//! configuration. It belongs to the user, not to a workspace. `ego-core` owns
//! only this pure, typed record and its revision lookup rule; filesystem
//! publication and the terminal live in `ego-cli`.

use ego_core::config::{Trust, TrustLedger, UNCONFINED_CONTRACT_REVISION, UnconfinedLedger};

const ROOT: &str = r"C:\work\alpha";

#[test]
fn an_empty_ledger_has_no_acknowledgement() {
    let ledger = UnconfinedLedger::default();

    assert!(!ledger.acknowledged(UNCONFINED_CONTRACT_REVISION));
}

// Catches: an acknowledgement of one contract silently covers a changed contract.
#[test]
fn an_acknowledgement_covers_only_its_contract_revision() {
    let mut ledger = UnconfinedLedger::default();
    ledger.acknowledge(UNCONFINED_CONTRACT_REVISION);

    assert!(ledger.acknowledged(UNCONFINED_CONTRACT_REVISION));
    assert!(
        !ledger.acknowledged(UNCONFINED_CONTRACT_REVISION + 1),
        "a changed host-risk contract requires a new acknowledgement"
    );

    ledger.acknowledge(UNCONFINED_CONTRACT_REVISION + 1);
    assert!(!ledger.acknowledged(UNCONFINED_CONTRACT_REVISION));
    assert!(ledger.acknowledged(UNCONFINED_CONTRACT_REVISION + 1));
}

#[test]
fn the_versioned_document_round_trips_without_workspace_or_trust_fields() {
    let mut ledger = UnconfinedLedger::default();
    ledger.acknowledge(UNCONFINED_CONTRACT_REVISION);

    let encoded = serde_json::to_value(&ledger).expect("one versioned document");
    assert_eq!(
        encoded,
        serde_json::json!({ "v": 1, "revision": UNCONFINED_CONTRACT_REVISION })
    );

    let decoded: UnconfinedLedger =
        serde_json::from_value(encoded).expect("a readable acknowledgement ledger");
    assert!(decoded.acknowledged(UNCONFINED_CONTRACT_REVISION));
}

#[test]
fn an_unconfined_document_is_not_a_trust_decision() {
    let mut unconfined = UnconfinedLedger::default();
    unconfined.acknowledge(UNCONFINED_CONTRACT_REVISION);
    let encoded = serde_json::to_value(&unconfined).expect("ledger is serialisable");

    assert!(
        serde_json::from_value::<TrustLedger>(encoded).is_err(),
        "the host acknowledgement must not deserialize as project trust"
    );

    let mut trust = TrustLedger::default();
    trust.remember(
        ROOT,
        ego_core::config::ExecutableSurface::default(),
        Trust::Trusted,
    );
    let trust_json = serde_json::to_value(&trust).expect("trust ledger is serialisable");
    assert!(
        serde_json::from_value::<UnconfinedLedger>(trust_json).is_err(),
        "project trust must not deserialize as a host acknowledgement"
    );
}
