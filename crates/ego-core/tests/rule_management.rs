//! contract for regression case's pure rule-management core seam.

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
fn rows_projects_the_admitted_rules_without_a_synthetic_user_snapshot() {
    let rules = system_rules(&["write(src/**)"], &["read(src/**)"]);
    let rows = RuleManagement::rows(&rules);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].effect, ManagedRuleEffect::Deny);
    assert_eq!(rows[0].rule, rule("read(src/**)"));
    assert_eq!(rows[0].from, Source::System);
    assert!(rows[0].also.is_empty());
    assert_eq!(rows[1].effect, ManagedRuleEffect::Allow);
    assert_eq!(rows[1].rule, rule("write(src/**)"));
    assert_eq!(rows[1].from, Source::System);
    assert!(rows[1].also.is_empty());
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
fn compare_resolved_rulesets_preserves_storage_and_effective_authority_separately() {
    let system = system_rules(&["write(src/**)"], &["read(src/**)"]);
    let before = RuleManagement::effective(
        &system,
        &user_snapshot(&["read(src/**)"], &["read(src/**)"]),
    );
    let after = RuleManagement::effective(
        &system,
        &user_snapshot(&["read(src/private/**)"], &["read(src/**)"]),
    );

    let shadowed = RuleManagement::compare_resolved(
        &before,
        &before,
        true,
        &rule("read(src/**)"),
        ManagedRuleEffect::Allow,
    );
    assert!(shadowed.stored_changed);
    assert!(!shadowed.effective_changed);
    assert_eq!(shadowed.authorities.len(), 1);
    assert_eq!(shadowed.authorities[0].from, Source::System);
    assert_eq!(shadowed.authorities[0].id, rule("read(src/**)").id());
    assert_eq!(
        shadowed.authorities[0].also,
        OriginSet::from(vec![Source::User])
    );

    let unchanged_storage = RuleManagement::compare_resolved(
        &before,
        &after,
        false,
        &rule("read(src/private/**)"),
        ManagedRuleEffect::Allow,
    );
    assert!(!unchanged_storage.stored_changed);
    assert!(unchanged_storage.effective_changed);

    let changed = RuleManagement::compare_resolved(
        &before,
        &after,
        true,
        &rule("read(src/private/**)"),
        ManagedRuleEffect::Allow,
    );
    assert!(changed.stored_changed);
    assert!(changed.effective_changed);
    assert!(changed.authorities.is_empty());

    let rows = RuleManagement::rows(&after);
    let deny = rows
        .iter()
        .find(|row| row.effect == ManagedRuleEffect::Deny)
        .expect("system deny remains in the resolved ruleset");
    assert_eq!(deny.id, rule("read(src/**)").id());
    assert_eq!(deny.effect, ManagedRuleEffect::Deny);
    assert_eq!(deny.from, Source::System);
    assert_eq!(deny.also, OriginSet::from(vec![Source::User]));
}

#[test]
fn explain_denied_call_stops_at_first_deny_and_reports_only_that_authority() {
    let rules = system_rules(&["read(src/**)"], &["read(private/**)"]);
    let call = PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new("/workspace/private/file.txt"),
        },
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
        Op::Read {
            path: CanonPath::new("/workspace/src/main.rs"),
        },
    )
    .with_op(Op::Read {
        path: CanonPath::new("/workspace/other.txt"),
    });
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
        Op::Read {
            path: CanonPath::new("/workspace/src/main.rs"),
        },
    )
    .with_op(Op::Write {
        path: CanonPath::new("/workspace/src/main.rs"),
        change: Change::Whole(b"updated".to_vec()),
    })
    .with_op(Op::Read {
        path: CanonPath::new("/workspace/src/main.rs"),
    });
    let plan =
        RuleManagement::remember(&call, ManagedRuleEffect::Allow, Path::new("/workspace")).unwrap();
    assert_eq!(plan.candidates.len(), 2);
    assert_eq!(plan.candidates[0], rule("read(/workspace/src/main.rs)"));
    assert!(plan.omitted.contains(&OmittedDimension::Change));
    let outside = PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new("/outside/secret.txt"),
        },
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
    let evidence_call = PreparedCall::new("read_evidence", Op::ReadEvidence { request: evidence });
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
        &evidence_call.ops()[0],
        secret_call
            .ops()
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

fn exec_op(argv: &[&str]) -> Op {
    Op::Spawn {
        spec: ego_core::permit::SpawnSpec::new(
            argv.iter().map(std::ffi::OsString::from).collect(),
            None,
            None,
            ego_core::permit::EnvPolicy::Inherited,
        )
        .unwrap(),
    }
}

fn exec_call(argv: &[&str]) -> PreparedCall {
    PreparedCall::new("exec", exec_op(argv))
}

fn suggested(call: &PreparedCall) -> Vec<Rule> {
    RuleManagement::suggest_remember(call, ManagedRuleEffect::Allow, Path::new("/workspace"))
        .unwrap()
        .candidates
}

/// Catches an exact-argv rule that can never match the next call: remembering
/// `python3 -c <this code>` must also cover `python3 -c <other code>`.
#[test]
fn suggested_exec_rule_is_the_argv_prefix_and_matches_the_next_call() {
    let rules = suggested(&exec_call(&["python3", "-c", "print(1)"]));
    assert_eq!(rules, vec![rule("exec(python3 *)")]);
    assert!(rules[0].matches(
        &exec_op(&["python3", "-c", "print(2)"]),
        Path::new("/workspace")
    ));
    assert!(!rules[0].matches(&exec_op(&["python", "-c", "x"]), Path::new("/workspace")));
}

/// Catches `git *` standing for `git -c core.sshCommand=... fetch`: a
/// subcommand program keeps its subcommand, and an option is not one.
#[test]
fn suggested_exec_rule_for_a_subcommand_program_keeps_the_subcommand() {
    let rules = suggested(&exec_call(&["git", "commit", "-m", "x"]));
    assert_eq!(rules, vec![rule("exec(git commit *)")]);
    let root = Path::new("/workspace");
    assert!(rules[0].matches(&exec_op(&["git", "commit", "-m", "y"]), root));
    assert!(!rules[0].matches(&exec_op(&["git", "push"]), root));
    assert!(!rules[0].matches(
        &exec_op(&["git", "-c", "core.sshCommand=x", "commit"]),
        root
    ));
    assert_eq!(
        suggested(&exec_call(&["git", "-C", "..", "status"])),
        vec![rule("exec(git *)")],
        "an option is never taken for the subcommand"
    );
}

/// Catches a prefix rule that authorises arbitrary code: a launcher, a path,
/// or a name carrying shell metacharacters keeps its exact form, which a
/// different command cannot match.
#[test]
fn suggested_exec_rule_falls_back_to_the_exact_form_for_launchers_and_metacharacters() {
    let root = Path::new("/workspace");
    let hostile = exec_op(&["bash", "-c", "rm -rf /"]);
    for argv in [
        vec!["bash", "-c", "echo hi; echo there"],
        vec!["sh", "-c", "true"],
        vec!["env", "FOO=1", "ls"],
        vec!["sudo", "rm", "x"],
        vec!["xargs", "rm"],
        vec!["/bin/ls", "-l"],
        vec!["./run.sh"],
        vec!["ls;rm", "x"],
        vec!["$(evil)", "x"],
        vec!["py*", "x"],
    ] {
        let call = exec_call(&argv);
        let exact = RuleManagement::remember(&call, ManagedRuleEffect::Allow, root)
            .unwrap()
            .candidates;
        let rules = suggested(&call);
        assert_eq!(rules, exact, "{argv:?} was widened");
        assert!(
            !rules[0].matches(&hostile, root),
            "{argv:?} authorises {hostile:?}"
        );
    }
}

/// Catches remember widening beyond the workspace: the write rule is the
/// workspace glob, not a path, a parent, or a root.
#[test]
fn suggested_write_rule_is_the_workspace_glob_and_never_wider() {
    let call = PreparedCall::new(
        "write",
        Op::Write {
            path: CanonPath::new("/workspace/src/main.rs"),
            change: Change::Whole(b"x".to_vec()),
        },
    );
    let plan =
        RuleManagement::suggest_remember(&call, ManagedRuleEffect::Allow, Path::new("/workspace"))
            .unwrap();
    assert_eq!(plan.candidates, vec![rule("write(/workspace/**)")]);
    assert_eq!(plan.omitted, vec![OmittedDimension::Change]);
    let write = |path: &str| Op::Write {
        path: CanonPath::new(path),
        change: Change::Whole(Vec::new()),
    };
    let root = Path::new("/workspace");
    assert!(plan.candidates[0].matches(&write("/workspace/docs/new.md"), root));
    assert!(!plan.candidates[0].matches(&write("/workspace-other/x"), root));
    assert!(!plan.candidates[0].matches(&write("/other/x"), root));

    let outside = PreparedCall::new(
        "write",
        Op::Write {
            path: CanonPath::new("/other/x"),
            change: Change::Whole(Vec::new()),
        },
    );
    assert!(matches!(
        RuleManagement::suggest_remember(&outside, ManagedRuleEffect::Allow, root),
        Err(RuleManagementError::OutsideWorkspace { .. })
    ));
    assert!(
        matches!(
            RuleManagement::suggest_remember(&call, ManagedRuleEffect::Allow, Path::new("/")),
            Err(RuleManagementError::NonPersistible(_))
        ),
        "a filesystem-root workspace must never become write(/**)"
    );
}

/// Catches a remembered prefix overriding a system deny: the broader allow
/// is only an allow, and deny precedence is evaluated first.
#[test]
fn a_suggested_rule_cannot_lift_a_system_deny() {
    use ego_core::config::Mode;
    use ego_core::permit::{Classification, PolicyDenied, PolicyEngine};

    let mut rules = system_rules(&[], &["write(/workspace/secrets/**)"]);
    let plan = RuleManagement::suggest_remember(
        &PreparedCall::new(
            "write",
            Op::Write {
                path: CanonPath::new("/workspace/notes.md"),
                change: Change::Whole(Vec::new()),
            },
        ),
        ManagedRuleEffect::Allow,
        Path::new("/workspace"),
    )
    .unwrap();
    Ruleset::merge(&mut rules.allow, plan.candidates[0].clone(), Source::User);
    let engine = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Default, rules);

    let secret = PreparedCall::new(
        "write",
        Op::Write {
            path: CanonPath::new("/workspace/secrets/key"),
            change: Change::Whole(Vec::new()),
        },
    );
    assert!(matches!(
        engine.classify(secret),
        Classification::Settled(Err(PolicyDenied::Denied { .. }))
    ));
}
