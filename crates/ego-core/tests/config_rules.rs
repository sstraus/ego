//! contract for persistent user-owned permission rules.

use std::str::FromStr;

use ego_core::config::{
    ManagedRuleEffect, RuleMutation, RuleMutationFacts, RuleSnapshot, UserConfigMutation,
    UserConfigTarget,
};
use ego_core::permit::rule::{Rule, RuleId};

fn rule(raw: &str) -> Rule {
    Rule::load(raw).expect("fixture rule is enforceable")
}

#[test]
fn rule_snapshot_preserves_exact_target_presence_order_and_effect() {
    let target = UserConfigTarget::Base;
    let read = rule("read(src/**)");
    let exec = rule("exec(cargo *)");
    let snapshot = RuleSnapshot {
        target: target.clone(),
        permissions_present: true,
        allow: vec![read.clone()],
        deny: vec![exec.clone()],
    };
    assert_eq!(snapshot.target, target);
    assert!(snapshot.permissions_present);
    assert_eq!(snapshot.allow, vec![read]);
    assert_eq!(snapshot.deny, vec![exec]);
}

#[test]
fn one_batch_carries_typed_effect_rules_and_revoke_uses_rule_id_only() {
    let target = UserConfigTarget::Profile("dev".parse().unwrap());
    let read = rule("read(src/**)");
    let changes = vec![
        RuleMutation {
            effect: ManagedRuleEffect::Allow,
            rule: read.clone(),
        },
        RuleMutation {
            effect: ManagedRuleEffect::Deny,
            rule: rule("write(src/**)"),
        },
    ];
    let batch = UserConfigMutation::Rules {
        target: target.clone(),
        changes: changes.clone(),
    };
    assert_eq!(batch.target(), target);
    assert_eq!(batch.facts(), RuleMutationFacts::Batch { changes });
    let revoke = UserConfigMutation::RevokeRule {
        target,
        id: read.id(),
    };
    assert!(matches!(revoke, UserConfigMutation::RevokeRule { id, .. } if id == read.id()));
}

#[test]
fn store_port_exposes_exact_rule_snapshot_and_typed_results() {
    fn read(
        store: &dyn ego_core::config::UserConfigStore,
        target: UserConfigTarget,
    ) -> Result<RuleSnapshot, ego_core::config::UserConfigError> {
        store.rule_snapshot(target)
    }
    let _ = read;
    let _ = RuleId::from_str("r1:000000000000");
}

#[test]
fn rule_ids_are_predicate_ids_and_effect_conflicts_are_not_silent() {
    let predicate = rule("read(src/**)");
    let same = RuleMutation {
        effect: ManagedRuleEffect::Allow,
        rule: predicate.clone(),
    };
    let opposite = RuleMutation {
        effect: ManagedRuleEffect::Deny,
        rule: predicate,
    };
    assert_eq!(same.rule.id(), opposite.rule.id());
    assert_ne!(same.effect, opposite.effect);
}
