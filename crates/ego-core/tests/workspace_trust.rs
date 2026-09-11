//! contract for regression case: a remembered trust answer, and what it is an
//! answer about.
//!
//! Everything here is pure. Which paths get probed and how the ledger reaches
//! `$EGO_HOME` belong to the binary; what an inventory *is*, when two of them
//! are the same, and whether a stored answer still applies are decisions, and
//! decisions live in core. See
//! `docs/decisions/2026-09-05-a-trust-answer-is-remembered-per-root.md`.

use ego_core::config::{Trust, TrustInventory, TrustLedger, TrustedContent};

fn file(path: &str, bytes: &[u8]) -> TrustedContent {
    TrustedContent::regular(path, bytes.len() as u64, bytes)
}

#[test]
fn an_empty_inventory_is_a_workspace_with_nothing_to_trust() {
    assert!(TrustInventory::new(Vec::new()).is_empty());
    assert!(!TrustInventory::new(vec![file("AGENTS.md", b"hi")]).is_empty());
}

#[test]
fn the_digest_does_not_depend_on_the_order_files_were_found_in() {
    let one = TrustInventory::new(vec![
        file("AGENTS.md", b"hi"),
        file(".ego/config.toml", b"model = 'x'"),
    ]);
    let other = TrustInventory::new(vec![
        file(".ego/config.toml", b"model = 'x'"),
        file("AGENTS.md", b"hi"),
    ]);

    assert_eq!(one.digest(), other.digest());
}

#[test]
fn editing_adding_or_removing_any_trusted_file_changes_the_digest() {
    let base = TrustInventory::new(vec![file("AGENTS.md", b"hi")]);
    let edited = TrustInventory::new(vec![file("AGENTS.md", b"ho")]);
    let added = TrustInventory::new(vec![
        file("AGENTS.md", b"hi"),
        file(".ego/skills/x/SKILL.md", b"---\nname: x\n---\n"),
    ]);

    assert_ne!(base.digest(), edited.digest(), "an edit re-asks");
    assert_ne!(base.digest(), added.digest(), "a new skill re-asks");
    assert_ne!(
        added.digest(),
        TrustInventory::new(vec![file(".ego/skills/x/SKILL.md", b"---\nname: x\n---\n")]).digest(),
        "a removed file re-asks"
    );
}

#[test]
fn the_same_bytes_under_a_different_name_are_a_different_inventory() {
    let one = TrustInventory::new(vec![file("AGENTS.md", b"hi")]);
    let other = TrustInventory::new(vec![file(".ego/config.toml", b"hi")]);

    assert_ne!(one.digest(), other.digest());
}

#[test]
fn an_empty_ledger_has_never_been_asked() {
    let ledger = TrustLedger::default();

    assert_eq!(ledger.remembered("/ws", "digest"), None);
}

#[test]
fn a_remembered_answer_is_honoured_only_against_the_digest_it_answered() {
    let mut ledger = TrustLedger::default();
    ledger.remember("/ws", "before", Trust::Trusted);

    assert_eq!(ledger.remembered("/ws", "before"), Some(Trust::Trusted));
    assert_eq!(
        ledger.remembered("/ws", "after"),
        None,
        "changed content re-asks"
    );
    assert_eq!(
        ledger.remembered("/elsewhere", "before"),
        None,
        "a remembered root is not a remembered machine"
    );
}

#[test]
fn a_refusal_is_remembered_as_a_refusal() {
    let mut ledger = TrustLedger::default();
    ledger.remember("/ws", "digest", Trust::Untrusted);

    assert_eq!(
        ledger.remembered("/ws", "digest"),
        Some(Trust::Untrusted),
        "`no` is an answer, not the absence of one"
    );
}

#[test]
fn re_answering_replaces_the_record_rather_than_adding_one() {
    let mut ledger = TrustLedger::default();
    ledger.remember("/ws", "before", Trust::Trusted);
    ledger.remember("/ws", "after", Trust::Untrusted);

    assert_eq!(ledger.remembered("/ws", "before"), None);
    assert_eq!(ledger.remembered("/ws", "after"), Some(Trust::Untrusted));
}

#[test]
fn the_ledger_round_trips_through_its_versioned_document() {
    let mut ledger = TrustLedger::default();
    ledger.remember("/ws", "digest", Trust::Trusted);

    let encoded = serde_json::to_value(&ledger).expect("one versioned document");
    assert_eq!(encoded["v"], 1);
    assert_eq!(encoded["workspaces"]["/ws"]["decision"], "trusted");
    assert_eq!(encoded["workspaces"]["/ws"]["digest"], "digest");

    let decoded: TrustLedger = serde_json::from_value(encoded).expect("a readable ledger");
    assert_eq!(decoded.remembered("/ws", "digest"), Some(Trust::Trusted));
}
