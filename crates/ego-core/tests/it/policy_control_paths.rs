//! Allow-rule guard at the canonical policy boundary (regression).

use std::path::Path;

use ego_core::config::rule_management::{RuleManagement, RuleOutcome};
use ego_core::config::{ManagedRuleEffect, Mode, Ruleset, Source};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    CanonPath, Change, Classification, Op, PolicyDenied, PolicyEngine, PreparedCall, Resolution,
};
use futures::executor::block_on;

fn write(path: &str) -> PreparedCall {
    PreparedCall::new(
        "write",
        Op::Write {
            path: CanonPath::new(path),
            change: Change::Whole(b"x".to_vec()),
        },
    )
}

/// Catches non-owner or broad rules gaining access to nested controls.
#[test]
fn policy_control_writes_require_exact_user_rules() {
    for source in Source::ALL {
        for path in [
            "/workspace/.git",
            "/workspace/.git/hooks/x",
            "/workspace/.git/config",
            "/workspace/.ego/x",
            "/workspace/.claude/settings.json",
            "/workspace/vendor/x/.git/hooks/y",
            "/workspace/vendor/x/.ego/x",
            "/workspace/vendor/x/.claude/settings.json",
        ] {
            for pattern in ["write(/workspace/**)".to_owned(), format!("write({path})")] {
                let mut rules = Ruleset::default();
                Ruleset::merge(&mut rules.allow, Rule::load(&pattern).unwrap(), source);
                if source == Source::User && !pattern.contains('*') {
                    assert!(matches!(
                        PolicyEngine::new(
                            CanonPath::new("/workspace"),
                            Mode::Default,
                            rules.clone()
                        )
                        .classify(write(path)),
                        Classification::Settled(Ok(_))
                    ));
                    assert_eq!(
                        RuleManagement::explain(&write(path), &rules, Path::new("/workspace"))
                            .outcome,
                        RuleOutcome::Allowed
                    );
                    continue;
                }
                let explanation =
                    RuleManagement::explain(&write(path), &rules, Path::new("/workspace"));
                assert_eq!(
                    explanation.outcome,
                    RuleOutcome::Uncovered,
                    "{source}: {pattern}"
                );
                assert!(explanation.authorities.is_empty());
                assert_eq!(explanation.uncovered, vec![0]);
                let engine = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Default, rules);
                let Classification::Ask(pending) = engine.classify(write(path)) else {
                    panic!("control write must ask: {source}: {pattern}");
                };
                assert!(matches!(
                    block_on(engine.settle(pending)),
                    Resolution::Ask(_)
                ));
            }
        }
    }
}

/// Catches applying the allow guard to deny matching or letting an allow from
/// another source override the explicit system deny, including a multi-op call.
#[test]
fn policy_control_denies_still_win_before_any_uncovered_operation() {
    for source in Source::ALL {
        let mut rules = Ruleset::default();
        Ruleset::merge(
            &mut rules.allow,
            Rule::load("write(/workspace/vendor/x/.git/hooks/y)").unwrap(),
            source,
        );
        Ruleset::merge(
            &mut rules.deny,
            Rule::load("write(/workspace/**/.git/**)").unwrap(),
            Source::System,
        );
        let engine = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Default, rules.clone());
        let call = write("/workspace/.ego/x")
            .with_op(write("/workspace/vendor/x/.git/hooks/y").ops()[0].clone());
        assert_eq!(
            RuleManagement::explain(&call, &rules, Path::new("/workspace")).outcome,
            RuleOutcome::Denied
        );
        assert!(matches!(
            engine.classify(call),
            Classification::Settled(Err(PolicyDenied::Denied {
                from: Source::System,
                ..
            }))
        ));
    }
}

/// Catches substring-based exclusion of ordinary files or a guard that covers
/// only writes even though no filesystem allow rule may cover a control path.
#[test]
fn policy_control_component_guard_covers_all_filesystem_rule_heads() {
    for path in [
        "/workspace/.git/config",
        "/workspace/vendor/.ego/x",
        "/workspace/vendor/.claude/settings.json",
        "/workspace/.GIT/config",
        "/workspace/.Git/hooks/x",
        "/workspace/vendor/.Ego/x",
        "/workspace/vendor/.CLAUDE/settings.json",
        "/workspace/src/.gitignore",
        "/workspace/src/.ego-backup/x",
        "/workspace/src/.claude.json",
    ] {
        let expected_ask = matches!(
            path,
            "/workspace/.git/config"
                | "/workspace/vendor/.ego/x"
                | "/workspace/vendor/.claude/settings.json"
                | "/workspace/.GIT/config"
                | "/workspace/.Git/hooks/x"
                | "/workspace/vendor/.Ego/x"
                | "/workspace/vendor/.CLAUDE/settings.json"
        );
        for (head, op) in [
            (
                "read",
                Op::Read {
                    path: CanonPath::new(path),
                },
            ),
            ("write", write(path).ops()[0].clone()),
            (
                "list",
                Op::List {
                    root: CanonPath::new(path),
                    pattern: "*".to_owned(),
                },
            ),
            (
                "search",
                Op::Search {
                    root: CanonPath::new(path),
                    query: "x".to_owned(),
                },
            ),
        ] {
            let mut rules = Ruleset::default();
            Ruleset::merge(
                &mut rules.allow,
                Rule::load(&format!("{head}(/workspace/**)")).unwrap(),
                Source::Project,
            );
            let engine = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Default, rules);
            let classified = engine.classify(PreparedCall::new(head, op));
            if expected_ask {
                assert!(
                    matches!(classified, Classification::Ask(_)),
                    "{head}: {path}"
                );
            } else {
                assert!(
                    matches!(classified, Classification::Settled(Ok(_))),
                    "{head}: {path}"
                );
            }
        }
    }
}

/// Catches exact owner control reads still asking on each invocation.
#[test]
fn exact_user_control_path_rule_settles_repeated_read() {
    let mut rules = Ruleset::default();
    let call = PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new("/workspace/.git/config"),
        },
    );
    for rule in RuleManagement::remember(&call, ManagedRuleEffect::Allow, Path::new("/workspace"))
        .unwrap()
        .candidates
    {
        Ruleset::merge(&mut rules.allow, rule, Source::User);
    }
    let engine = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Default, rules);
    for _ in 0..2 {
        assert!(
            matches!(
                engine.classify(PreparedCall::new(
                    "read",
                    Op::Read {
                        path: CanonPath::new("/workspace/.git/config")
                    }
                )),
                Classification::Settled(Ok(_))
            ),
            "exact owner read must settle"
        );
    }
}

/// Catches repository provenance or an owner glob widening protected access.
#[test]
fn project_or_wildcard_rule_cannot_allow_control_paths() {
    for (source, pattern) in [
        (Source::Project, "read(/workspace/.git/config)"),
        (Source::User, "read(/workspace/**)"),
        (Source::User, "read(/workspace/.git/conf*)"),
    ] {
        let mut rules = Ruleset::default();
        Ruleset::merge(&mut rules.allow, Rule::load(pattern).unwrap(), source);
        let engine = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Default, rules);
        assert!(
            matches!(
                engine.classify(PreparedCall::new(
                    "read",
                    Op::Read {
                        path: CanonPath::new("/workspace/.git/config")
                    }
                )),
                Classification::Ask(_)
            ),
            "{source}: {pattern}"
        );
    }
}

/// Catches exact owner rules bypassing Plan, escaping roots or explicit denies.
#[test]
fn exact_owner_control_rules_keep_denies_modes_and_scope() {
    for (mode, denied, path) in [
        (Mode::Plan, false, "/workspace/.git/config"),
        (Mode::Default, true, "/workspace/.git/config"),
        (Mode::Default, false, "/outside/.git/config"),
    ] {
        let mut rules = Ruleset::default();
        Ruleset::merge(
            &mut rules.allow,
            Rule::load(&format!("write({path})")).unwrap(),
            Source::User,
        );
        if denied {
            Ruleset::merge(
                &mut rules.deny,
                Rule::load(&format!("write({path})")).unwrap(),
                Source::User,
            );
        }
        let engine = PolicyEngine::new(CanonPath::new("/workspace"), mode, rules);
        assert!(matches!(
            engine.classify(write(path)),
            Classification::Settled(Err(_))
        ));
    }
}
