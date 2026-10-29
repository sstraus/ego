//! contract for regression case: a remembered trust answer, and what it is an
//! answer about.
//!
//! Everything here is pure. Which paths get probed and how the ledger reaches
//! `$EGO_HOME` belong to the binary; what an inventory *is*, when two of them
//! are the same, and whether a stored answer still applies are decisions, and
//! decisions live in core. See
//! `docs/architecture.md`.

use ego_core::config::{
    ExecutableDeclaration, ExecutableSurface, ProgramEvidence, Trust, TrustInventory, TrustLedger,
    TrustedContent,
};

fn surface(label: &str) -> ExecutableSurface {
    let mut value = ExecutableSurface::default();
    value.programs.insert(
        "handler.sh".to_owned(),
        ProgramEvidence {
            bytes: 10,
            digest: label.to_owned(),
            executable: true,
            link_target: None,
        },
    );
    value
}

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

    assert_eq!(ledger.remembered("/ws", &surface("digest")), None);
}

#[test]
fn a_remembered_answer_is_honoured_only_against_the_digest_it_answered() {
    let mut ledger = TrustLedger::default();
    ledger.remember("/ws", surface("before"), Trust::Trusted);

    assert_eq!(
        ledger.remembered("/ws", &surface("before")),
        Some(Trust::Trusted)
    );
    assert_eq!(
        ledger.remembered("/ws", &surface("after")),
        None,
        "changed content re-asks"
    );
    assert_eq!(
        ledger.remembered("/elsewhere", &surface("before")),
        None,
        "a remembered root is not a remembered machine"
    );
}

#[test]
fn a_refusal_is_remembered_as_a_refusal() {
    let mut ledger = TrustLedger::default();
    ledger.remember("/ws", surface("digest"), Trust::Untrusted);

    assert_eq!(
        ledger.remembered("/ws", &surface("digest")),
        Some(Trust::Untrusted),
        "`no` is an answer, not the absence of one"
    );
}

#[test]
fn re_answering_replaces_the_record_rather_than_adding_one() {
    let mut ledger = TrustLedger::default();
    ledger.remember("/ws", surface("before"), Trust::Trusted);
    ledger.remember("/ws", surface("after"), Trust::Untrusted);

    assert_eq!(ledger.remembered("/ws", &surface("before")), None);
    assert_eq!(
        ledger.remembered("/ws", &surface("after")),
        Some(Trust::Untrusted)
    );
}

#[test]
fn the_ledger_round_trips_through_its_versioned_document() {
    let mut ledger = TrustLedger::default();
    ledger.remember("/ws", surface("digest"), Trust::Trusted);

    let encoded = serde_json::to_value(&ledger).expect("one versioned document");
    assert_eq!(encoded["v"], 2);
    assert_eq!(encoded["workspaces"]["/ws"]["decision"], "trusted");
    assert_eq!(
        encoded["workspaces"]["/ws"]["executable_surface"]["programs"]["handler.sh"]["digest"],
        "digest"
    );

    let decoded: TrustLedger = serde_json::from_value(encoded).expect("a readable ledger");
    assert_eq!(
        decoded.remembered("/ws", &surface("digest")),
        Some(Trust::Trusted)
    );
}

#[test]
fn legacy_digest_only_records_keep_their_decision() {
    let ledger: TrustLedger = serde_json::from_value(serde_json::json!({
        "v": 1,
        "workspaces": {"/ws": {"decision": "trusted", "digest": "old-inventory"}}
    }))
    .expect("old ledger");

    assert_eq!(
        ledger.remembered("/ws", &surface("current-executable")),
        Some(Trust::Trusted)
    );
}

#[test]
fn executable_change_summary_names_only_changed_entries_without_environment_values() {
    let mut before = ExecutableSurface::default();
    let old = ExecutableDeclaration {
        config: ".ego/config.toml".to_owned(),
        kind: "MCP server".to_owned(),
        name: "local".to_owned(),
        command: "./server".to_owned(),
        args: vec!["--safe".to_owned()],
        env_keys: vec!["TOKEN".to_owned()],
        details: Default::default(),
    };
    before
        .declarations
        .insert("mcp:local".to_owned(), old.clone());
    let mut stable = old.clone();
    stable.name = "unchanged".to_owned();
    before
        .declarations
        .insert("mcp:unchanged".to_owned(), stable);
    before.programs.insert(
        "server".to_owned(),
        ProgramEvidence {
            bytes: 10,
            digest: "before".to_owned(),
            executable: false,
            link_target: None,
        },
    );
    let mut after = before.clone();
    after.declarations.get_mut("mcp:local").expect("local").args = vec!["--danger".to_owned()];
    after.programs.get_mut("server").expect("program").bytes = 20;
    after
        .programs
        .get_mut("server")
        .expect("program")
        .executable = true;
    let summary = after.changes_from(&before);
    assert!(summary.contains(".ego/config.toml"), "{summary}");
    assert!(
        summary.contains("--safe") && summary.contains("--danger"),
        "{summary}"
    );
    assert!(summary.contains("TOKEN"), "{summary}");
    assert!(
        summary.contains("size=10 executable=false") && summary.contains("size=20 executable=true"),
        "{summary}"
    );
    assert!(!summary.contains("unchanged"), "{summary}");
}

/// Catches sharing executable trust without binding external disclosure bytes.
#[test]
fn external_instruction_consent_is_bound_to_target_and_content() {
    let mut ledger = TrustLedger::default();
    let mut offered = ExecutableSurface::default();
    offered
        .external_instructions
        .insert("/home/shared.md".into(), "digest-a".into());
    ledger.remember("/workspace", offered.clone(), Trust::Trusted);
    let persisted = serde_json::to_vec(&ledger).unwrap();
    let loaded: TrustLedger = serde_json::from_slice(&persisted).unwrap();
    assert_eq!(
        loaded.remembered("/workspace", &offered),
        Some(Trust::Trusted)
    );
    let original = offered.clone();
    offered
        .external_instructions
        .insert("/home/shared.md".into(), "digest-b".into());
    assert_eq!(loaded.remembered("/workspace", &offered), None);
    assert!(offered.changes_from(&original).contains("/home/shared.md"));
    let digest = offered
        .external_instructions
        .remove("/home/shared.md")
        .unwrap();
    offered
        .external_instructions
        .insert("/other/shared.md".into(), digest);
    assert_eq!(loaded.remembered("/workspace", &offered), None);
    ledger.remember("/workspace", offered.clone(), Trust::Untrusted);
    assert_eq!(
        ledger.remembered("/workspace", &offered),
        Some(Trust::Untrusted)
    );
}

/// Catches legacy approval authorizing external disclosure or a record with no digest.
#[test]
fn legacy_trust_requires_a_digest_and_no_external_instruction_disclosure() {
    let ledger: TrustLedger = serde_json::from_value(serde_json::json!({
        "v": 1,
        "workspaces": {
            "/approved": {"decision": "trusted", "digest": "old-inventory"},
            "/incomplete": {"decision": "trusted"}
        }
    }))
    .expect("legacy ledger documents deserialize through the public contract");
    let mut offered = ExecutableSurface::default();
    assert_eq!(
        ledger.remembered("/approved", &offered),
        Some(Trust::Trusted)
    );
    assert_eq!(
        ledger.remembered("/incomplete", &offered),
        None,
        "absence of external instructions cannot replace a missing legacy digest"
    );
    offered.external_instructions.insert(
        "/shared/AGENTS.md".into(),
        TrustInventory::new([file("AGENTS.md", b"hello")]).digest(),
    );
    assert_eq!(
        ledger.remembered("/approved", &offered),
        None,
        "a legacy digest never approves disclosure of external instruction bytes"
    );
}
