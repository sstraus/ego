//! RED contract for regression case's pure rule-management core seam.

use std::path::Path;

use ego_core::config::{
    ManagedRuleEffect, OriginSet, RuleSnapshot, Ruleset, Source, UserConfigTarget,
};
use ego_core::event::hook::HookId;
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    CanonPath, Change, HttpRequest, HttpRoute, HttpScheme, NetworkTarget, Op, PreparedCall,
    RuleAuthority,
};
use ego_core::store::evidence::{EvidenceId, EvidenceRead, EvidenceRef};

use ego_core::config::rule_management::{
    EffectiveRule, OmittedDimension, RuleExplanation, RuleManagement, RuleManagementError,
    RuleMutationComparison, RuleOutcome,
};

fn rule(raw: &str) -> Rule {
    Rule::load(raw).unwrap()
}

fn user_snapshot(allow: &[&str], deny: &[&str]) -> RuleSnapshot {
    RuleSnapshot {
        target: UserConfigTarget::Base,
        permissions_present: true,
        allow: allow.iter().map(|raw| rule(raw)).collect(),
        deny: deny.iter().map(|raw| rule(raw)).collect(),
    }
}

fn system_rules(allow: &[&str], deny: &[&str]) -> Ruleset {
    let mut rules = Ruleset::default();
    for raw in allow {
        Ruleset::merge(&mut rules.allow, rule(raw), Source::System);
    }
    for raw in deny {
        Ruleset::merge(&mut rules.deny, rule(raw), Source::System);
    }
    rules
}

#[test]
fn effective_rows_fold_same_effect_copies_but_keep_opposite_effects_separate() {
    let system = system_rules(&["write(src/**)"], &["read(src/**)"]);
    let user = user_snapshot(&["read(src/**)", "write(src/**)"], &["read(src/**)"]);
    let rows: Vec<EffectiveRule> = RuleManagement::effective_rows(&system, &user);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].effect, ManagedRuleEffect::Deny);
    assert_eq!(rows[0].rule, rule("read(src/**)"));
    assert_eq!(rows[0].from, Source::System);
    assert_eq!(rows[0].also, OriginSet::from(vec![Source::User]));
    assert_eq!(rows[1].effect, ManagedRuleEffect::Allow);
    assert_eq!(rows[1].rule, rule("read(src/**)"));
    assert_eq!(rows[1].from, Source::User);
    assert!(rows[1].also.is_empty());
    assert_eq!(rows[2].effect, ManagedRuleEffect::Allow);
    assert_eq!(rows[2].rule, rule("write(src/**)"));
    assert_eq!(rows[2].from, Source::User);
    assert!(rows[2].also.is_empty());
    assert_eq!(rows[2].id, rule("write(src/**)").id());

    let absent = RuleSnapshot {
        target: UserConfigTarget::Base,
        permissions_present: false,
        allow: Vec::new(),
        deny: Vec::new(),
    };
    let retained: Vec<EffectiveRule> = RuleManagement::effective_rows(&system, &absent);
    assert_eq!(retained.len(), 2);
    assert_eq!(retained[0].from, Source::System);
    assert_eq!(retained[0].effect, ManagedRuleEffect::Deny);
    assert_eq!(retained[1].from, Source::System);
    assert_eq!(retained[1].effect, ManagedRuleEffect::Allow);
}

#[test]
fn compare_reports_one_authority_for_shadowed_and_overlapping_mutations() {
    let system = system_rules(&[], &["read(src/**)"]);
    let before = user_snapshot(&[], &[]);
    let shadowed = user_snapshot(&["read(src/**)"], &[]);
    let result: RuleMutationComparison = RuleManagement::compare(
        &system,
        &before,
        &shadowed,
        &rule("read(src/**)"),
        ManagedRuleEffect::Allow,
    );
    assert!(result.stored_changed);
    assert!(!result.effective_changed);
    assert_eq!(result.authorities.len(), 1);
    assert_eq!(result.authorities[0].from, Source::System);
    assert_eq!(result.authorities[0].id, rule("read(src/**)").id());
    let overlap = RuleManagement::compare(
        &system,
        &before,
        &user_snapshot(&[], &["read(src/private/**)"]),
        &rule("read(src/private/**)"),
        ManagedRuleEffect::Deny,
    );
    assert!(overlap.stored_changed);
    assert!(overlap.effective_changed);
    assert_eq!(overlap.authorities, Vec::<RuleAuthority>::new());
}

#[test]
fn explain_denied_call_stops_at_first_deny_and_reports_only_that_authority() {
    let rules = system_rules(&["read(src/**)"], &["read(private/**)"]);
    let call = PreparedCall::new(
        "read",
        vec![Op::Read {
            path: CanonPath::new("/workspace/private/file.txt"),
        }],
    );
    let explanation: RuleExplanation =
        RuleManagement::explain(&call, &rules, Path::new("/workspace"));
    assert_eq!(explanation.outcome, RuleOutcome::Denied);
    assert_eq!(explanation.operations[0].index, 0);
    assert_eq!(explanation.authorities.len(), 1);
    assert_eq!(explanation.authorities[0].operation, 0);
    assert_eq!(explanation.authorities[0].effect, ManagedRuleEffect::Deny);
    assert_eq!(explanation.authorities[0].from, Source::System);
    assert!(explanation.uncovered.is_empty());
}

#[test]
fn explain_without_deny_reports_first_allow_per_operation_and_uncovered_indices() {
    let rules = system_rules(&["read(src/**)"], &[]);
    let call = PreparedCall::new(
        "read",
        vec![
            Op::Read {
                path: CanonPath::new("/workspace/src/main.rs"),
            },
            Op::Read {
                path: CanonPath::new("/workspace/other.txt"),
            },
        ],
    );
    let explanation = RuleManagement::explain(&call, &rules, Path::new("/workspace"));
    assert_eq!(explanation.outcome, RuleOutcome::Uncovered);
    assert_eq!(explanation.authorities.len(), 1);
    assert_eq!(explanation.authorities[0].operation, 0);
    assert_eq!(explanation.authorities[0].effect, ManagedRuleEffect::Allow);
    assert_eq!(explanation.uncovered, vec![1]);
}

#[test]
fn remember_deduplicates_rules_uses_absolute_paths_and_refuses_omitted_or_outside_ops() {
    let call = PreparedCall::new(
        "edit",
        vec![
            Op::Read {
                path: CanonPath::new("/workspace/src/main.rs"),
            },
            Op::Write {
                path: CanonPath::new("/workspace/src/main.rs"),
                change: Change::Whole(b"updated".to_vec()),
            },
            Op::Read {
                path: CanonPath::new("/workspace/src/main.rs"),
            },
        ],
    );
    let plan =
        RuleManagement::remember(&call, ManagedRuleEffect::Allow, Path::new("/workspace")).unwrap();
    assert_eq!(plan.candidates.len(), 2);
    assert_eq!(plan.candidates[0], rule("read(/workspace/src/main.rs)"));
    assert!(plan.omitted.contains(&OmittedDimension::Change));
    let outside = PreparedCall::new(
        "read",
        vec![Op::Read {
            path: CanonPath::new("/outside/secret.txt"),
        }],
    );
    assert!(matches!(
        RuleManagement::remember(&outside, ManagedRuleEffect::Allow, Path::new("/workspace")),
        Err(RuleManagementError::OutsideWorkspace { .. })
    ));
    let evidence_id =
        EvidenceId::parse("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap();
    let evidence = EvidenceRead::new(
        EvidenceRef {
            id: evidence_id,
            bytes: 1,
        },
        0,
    )
    .unwrap();
    let evidence_call = PreparedCall::new(
        "read_evidence",
        vec![Op::ReadEvidence { request: evidence }],
    );
    let target = NetworkTarget::new("example.com", 443).unwrap();
    let route = HttpRoute::new(HttpScheme::Https, target).unwrap();
    let request = HttpRequest::get("/secret").unwrap();
    let secret_call = PreparedCall::hook_http(
        HookId::parse(&format!("h1:{}", "00".repeat(32))).unwrap(),
        route,
        &request,
        vec!["TOKEN".to_owned()],
    );
    for op in [
        &evidence_call.ops[0],
        secret_call
            .ops
            .iter()
            .find(|op| matches!(op, Op::SecretEgress { .. }))
            .unwrap(),
    ] {
        assert!(matches!(
            RuleManagement::remember_op(op, ManagedRuleEffect::Allow, Path::new("/workspace")),
            Err(RuleManagementError::NonPersistible(_))
        ));
    }
}
