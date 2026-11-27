//! Regression protection for story 624: remembered program rules, not shell scripts.
use crate::support;

use std::ffi::OsString;
use std::path::Path;

use ego_core::config::rule_management::{RuleManagement, RuleOutcome};
use ego_core::config::{ManagedRuleEffect, Mode, Ruleset, Source};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    CanonPath, Classification, EnvPolicy, Op, PolicyEngine, PreparedCall, SpawnSpec,
};

fn call(cwd: &str, argv: &[&str]) -> PreparedCall {
    PreparedCall::new(
        "exec",
        Op::Spawn {
            spec: SpawnSpec::new(
                argv.iter().map(OsString::from).collect(),
                Some(CanonPath::new(cwd)),
                None,
                EnvPolicy::exact(Vec::new()).unwrap(),
            )
            .unwrap(),
        },
    )
}

fn remembered(argv: &[&str]) -> Ruleset {
    let mut rules = Ruleset::default();
    for rule in RuleManagement::suggest_remember(
        &call(support::WORKSPACE, argv),
        ManagedRuleEffect::Allow,
        Path::new(support::WORKSPACE),
    )
    .unwrap()
    .candidates
    {
        Ruleset::merge(&mut rules.allow, rule, Source::User);
    }
    rules
}

fn allowed(rules: &Ruleset, cwd: &str, argv: &[&str]) -> bool {
    let call = call(cwd, argv);
    let actual = matches!(
        PolicyEngine::new(
            CanonPath::new(support::WORKSPACE),
            Mode::Default,
            rules.clone()
        )
        .classify(call.clone()),
        Classification::Settled(Ok(_))
    );
    assert_eq!(
        actual,
        RuleManagement::explain(&call, rules, Path::new(support::WORKSPACE)).outcome
            == RuleOutcome::Allowed
    );
    actual
}

// Catches: remembering the full shell string never covers the next read-only compound.
#[test]
fn remembered_compound_commands_reuse_program_rules() {
    let rules = remembered(&[
        "/bin/zsh",
        "-lc",
        "pwd; git status --short; git branch --show-current",
    ]);
    let text: Vec<_> = rules
        .allow
        .iter()
        .map(|entry| entry.rule().to_string())
        .collect();
    assert_eq!(
        text,
        ["exec(pwd)", "exec(git status *)", "exec(git branch *)"]
    );
    assert!(allowed(
        &rules,
        support::WORKSPACE,
        &[
            "/bin/zsh",
            "-lc",
            "git branch --list && git status --porcelain; pwd"
        ]
    ));
    assert!(allowed(
        &rules,
        support::WORKSPACE,
        &["git", "status", "--porcelain"]
    ));
    let direct = remembered(&["git", "status"]);
    assert!(allowed(
        &direct,
        support::WORKSPACE,
        &["bash", "-c", "git status --short; git status --porcelain"]
    ));
}

// Catches: one approved inner program masks unknown code, shell writes, or outside cwd.
#[test]
fn remembered_compound_rules_do_not_cover_unseen_effects() {
    let rules = remembered(&["git", "status"]);
    for script in [
        "git status; unknown",
        "git status > output",
        "git status $(unknown)",
        "TOKEN=value git status",
        "git status &",
        "(git status)",
        "git status; git reset --hard",
        "git status; cd /outside; git status",
        "git status; exec git status",
        "git status; source setup.sh",
        "git status |& git status",
        "git status <<< data",
        "git status; $(unknown)",
    ] {
        assert!(
            !allowed(&rules, support::WORKSPACE, &["bash", "-c", script]),
            "{script}"
        );
    }
    let outside = support::native("/outside");
    assert!(!allowed(
        &rules,
        outside.to_str().unwrap(),
        &["bash", "-c", "git status"]
    ));
}

// Catches: shell decomposition bypasses a system deny on an inner program.
#[test]
fn remembered_compound_rules_preserve_inner_system_denies() {
    let mut rules = remembered(&["git", "status"]);
    Ruleset::merge(
        &mut rules.deny,
        Rule::load("exec(git status *)").unwrap(),
        Source::System,
    );
    let call = call(support::WORKSPACE, &["bash", "-c", "git status --short"]);
    assert!(matches!(
        PolicyEngine::new(
            CanonPath::new(support::WORKSPACE),
            Mode::Default,
            rules.clone()
        )
        .classify(call.clone()),
        Classification::Settled(Err(_))
    ));
    assert_eq!(
        RuleManagement::explain(&call, &rules, Path::new(support::WORKSPACE)).outcome,
        RuleOutcome::Denied
    );
}

// Catches: unwrapping RTK loses its executable identity, or pnpm exec
// remembers every possible tool instead of the one shown to the operator.
#[test]
fn remembered_compound_wrappers_retain_the_selected_program() {
    let rules = remembered(&[
        "sh",
        "-c",
        "rtk git status --short; pnpm exec biome check src",
    ]);
    let text: Vec<_> = rules
        .allow
        .iter()
        .map(|entry| entry.rule().to_string())
        .collect();
    assert_eq!(text, ["exec(rtk git status *)", "exec(pnpm exec biome *)"]);
    assert!(allowed(
        &rules,
        support::WORKSPACE,
        &[
            "sh",
            "-c",
            "pnpm exec biome check tests; rtk git status --porcelain"
        ]
    ));
    assert!(!allowed(
        &rules,
        support::WORKSPACE,
        &["sh", "-c", "pnpm exec unknown; rtk git status"]
    ));
    assert!(!allowed(
        &rules,
        support::WORKSPACE,
        &["sh", "-c", "rtk git reset --hard"]
    ));
}

// Catches: [a] on a build or test tool outside the first list offers only the exact argv,
// so the next `make test`-style call asks again (story 956-a721).
#[test]
fn remember_offers_a_program_subcommand_rule_for_common_build_tools() {
    for (argv, rule) in [
        (&["make", "test", "-j4"][..], "exec(make test *)"),
        (&["just", "build"], "exec(just build *)"),
        (&["swift", "test", "--filter", "X"], "exec(swift test *)"),
        (
            &["dotnet", "build", "-c", "Release"],
            "exec(dotnet build *)",
        ),
        (&["gradle", "test"], "exec(gradle test *)"),
    ] {
        let rules = remembered(argv);
        let text: Vec<_> = rules.allow.iter().map(|e| e.rule().to_string()).collect();
        assert_eq!(text, [rule], "{argv:?}");
        assert!(allowed(&rules, support::WORKSPACE, argv), "{argv:?}");
    }
    let rules = remembered(&["make", "test"]);
    assert!(!allowed(&rules, support::WORKSPACE, &["make", "install"]));
}
