//! Contract for Windows unconfined-start acknowledgement.
//!
//! The acknowledgement is a host-risk fact, not project trust and not user
//! configuration.  `ego-core` owns only this pure, typed record and its
//! root/revision lookup rules; filesystem publication and the terminal live
//! in `ego-cli`.

use ego_core::config::{
    Trust, TrustLedger, UNCONFINED_CONTRACT_REVISION, UnconfinedLedger, UnconfinedRecord,
};

const FIRST_ROOT: &str = r"C:\work\alpha";
const SECOND_ROOT: &str = r"C:\work\beta";

#[test]
fn an_empty_ledger_has_no_acknowledgement() {
    let ledger = UnconfinedLedger::default();

    assert!(!ledger.acknowledged(FIRST_ROOT, UNCONFINED_CONTRACT_REVISION));
}

#[test]
fn an_acknowledgement_is_isolated_by_canonical_root_and_contract_revision() {
    let mut ledger = UnconfinedLedger::default();
    ledger.acknowledge(FIRST_ROOT, UNCONFINED_CONTRACT_REVISION);

    assert!(ledger.acknowledged(FIRST_ROOT, UNCONFINED_CONTRACT_REVISION));
    assert!(
        !ledger.acknowledged(SECOND_ROOT, UNCONFINED_CONTRACT_REVISION),
        "one workspace cannot acknowledge another workspace"
    );
    assert!(
        !ledger.acknowledged(FIRST_ROOT, UNCONFINED_CONTRACT_REVISION + 1),
        "a changed host-risk contract requires a new acknowledgement"
    );
}

#[test]
fn re_acknowledging_a_root_replaces_its_record_without_global_state() {
    let mut ledger = UnconfinedLedger::default();
    ledger.acknowledge(FIRST_ROOT, UNCONFINED_CONTRACT_REVISION);
    ledger.acknowledge(SECOND_ROOT, UNCONFINED_CONTRACT_REVISION);

    let encoded = serde_json::to_value(&ledger).expect("ledger is serialisable");
    let workspaces = encoded["workspaces"]
        .as_object()
        .expect("ledger has a workspace map");
    assert_eq!(workspaces.len(), 2);
    assert_eq!(
        workspaces[FIRST_ROOT]["revision"],
        UNCONFINED_CONTRACT_REVISION
    );
    assert_eq!(
        workspaces[SECOND_ROOT]["revision"],
        UNCONFINED_CONTRACT_REVISION
    );

    ledger.acknowledge(FIRST_ROOT, UNCONFINED_CONTRACT_REVISION + 1);
    assert!(!ledger.acknowledged(FIRST_ROOT, UNCONFINED_CONTRACT_REVISION));
    assert!(ledger.acknowledged(FIRST_ROOT, UNCONFINED_CONTRACT_REVISION + 1));
    assert!(ledger.acknowledged(SECOND_ROOT, UNCONFINED_CONTRACT_REVISION));
}

#[test]
fn the_versioned_document_round_trips_without_project_or_trust_fields() {
    let record = UnconfinedRecord {
        revision: UNCONFINED_CONTRACT_REVISION,
    };
    let mut ledger = UnconfinedLedger::default();
    ledger.acknowledge(FIRST_ROOT, record.revision);

    let encoded = serde_json::to_value(&ledger).expect("one versioned document");
    assert_eq!(encoded["v"], 1);
    assert_eq!(
        encoded["workspaces"][FIRST_ROOT]["revision"],
        record.revision
    );
    assert!(encoded["workspaces"][FIRST_ROOT].get("digest").is_none());
    assert!(encoded["workspaces"][FIRST_ROOT].get("decision").is_none());

    let decoded: UnconfinedLedger =
        serde_json::from_value(encoded).expect("a readable acknowledgement ledger");
    assert!(decoded.acknowledged(FIRST_ROOT, UNCONFINED_CONTRACT_REVISION));
}

#[test]
fn an_unconfined_document_is_not_a_trust_decision() {
    let mut unconfined = UnconfinedLedger::default();
    unconfined.acknowledge(FIRST_ROOT, UNCONFINED_CONTRACT_REVISION);
    let encoded = serde_json::to_value(&unconfined).expect("ledger is serialisable");

    assert!(
        serde_json::from_value::<TrustLedger>(encoded).is_err(),
        "the host acknowledgement must not deserialize as project trust"
    );

    let mut trust = TrustLedger::default();
    trust.remember(
        FIRST_ROOT,
        ego_core::config::ExecutableSurface::default(),
        Trust::Trusted,
    );
    let trust_json = serde_json::to_value(&trust).expect("trust ledger is serialisable");
    assert!(
        serde_json::from_value::<UnconfinedLedger>(trust_json).is_err(),
        "project trust must not deserialize as a host acknowledgement"
    );
}
