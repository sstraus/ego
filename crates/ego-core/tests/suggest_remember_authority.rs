//! Authority widening of the rules `suggest_remember` offers to a human who
//! answers `a` (regression, criterion 8). Each test names the bug it
//! catches; every assertion is about what the stored rule would later cover.

use std::ffi::OsString;
use std::path::Path;

use ego_core::config::rule_management::RuleManagement;
use ego_core::config::{ManagedRuleEffect, Mode, Ruleset, Source};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    CanonPath, Change, Classification, EnvPolicy, Op, PolicyEngine, PreparedCall, SpawnSpec,
};

const WORKSPACE: &str = "/workspace";

fn spawn(argv: &[&str]) -> PreparedCall {
    PreparedCall::new(
        "exec",
        Op::Spawn {
            spec: SpawnSpec::new(
                argv.iter().map(OsString::from).collect(),
                Some(CanonPath::new(WORKSPACE)),
                None,
                EnvPolicy::exact(Vec::new()).unwrap(),
            )
            .unwrap(),
        },
    )
}

fn write(path: &str) -> PreparedCall {
    PreparedCall::new(
        "write",
        Op::Write {
            path: CanonPath::new(path),
            change: Change::Whole(b"x".to_vec()),
        },
    )
}

fn suggested(call: &PreparedCall) -> Vec<Rule> {
    RuleManagement::suggest_remember(call, ManagedRuleEffect::Allow, Path::new(WORKSPACE))
        .expect("the call can be remembered")
        .candidates
}

/// Does the rule stored for `remembered` cover `later`?
fn covers(remembered: &[&str], later: &[&str]) -> bool {
    let later = spawn(later);
    suggested(&spawn(remembered)).iter().all(|rule| {
        later
            .ops()
            .iter()
            .all(|op| rule.matches(op, Path::new(WORKSPACE)))
    })
}

/// Catches `git -c k=v ...` being generalised to `exec(git *)`: `-c` is a
/// config injection (`core.sshCommand`, `core.pager`, `alias.x=!sh`), so the
/// rule would authorise arbitrary code, and a bare `git` would do the same.
#[test]
fn git_with_a_leading_option_never_becomes_the_all_git_rule() {
    assert!(!covers(
        &["git", "-c", "core.pager=cat", "log"],
        &["git", "-c", "core.sshCommand=evil", "fetch"],
    ));
    assert!(!covers(&["git"], &["git", "-c", "alias.x=!evil", "x"]));
}

/// Catches `cargo --config ...` / `cargo +toolchain ...` being turned into a
/// rule that also covers `cargo install`, `cargo run` or a rustc wrapper
/// override chosen later.
#[test]
fn cargo_options_and_toolchain_selectors_do_not_widen_to_every_subcommand() {
    assert!(!covers(
        &["cargo", "--config", "term.color=\"never\"", "build"],
        &[
            "cargo",
            "--config",
            "build.rustc-wrapper=\"/tmp/evil\"",
            "build"
        ],
    ));
    assert!(!covers(
        &["cargo", "+nightly", "build"],
        &["cargo", "+nightly", "install", "evil"],
    ));
}

/// Catches programs that run their arguments as code (interpreters, package
/// launchers, `find -exec`, `awk`, `sed e`) being offered as `exec(<prog> *)`,
/// which is `exec(anything)`. The wrapper fallback list names shells and
/// launchers only.
#[test]
fn programs_that_execute_their_arguments_are_not_offered_as_a_prefix() {
    let offenders: Vec<&str> = [
        "python3",
        "node",
        "deno",
        "bun",
        "perl",
        "ruby",
        "php",
        "lua",
        "awk",
        "gawk",
        "sed",
        "find",
        "make",
        "npx",
        "uvx",
        "pnpx",
        "osascript",
        "tclsh",
        "rsync",
        "tar",
        "open",
    ]
    .into_iter()
    .filter(|program| covers(&[program, "a"], &[program, "-e", "evil"]))
    .collect();
    assert!(
        offenders.is_empty(),
        "these would authorise arbitrary code after one `a`: {offenders:?}"
    );
}

// Catches: denying an explicitly named subcommand family, or dropping its
// subcommand and silently authorizing every action of the program.
#[test]
fn supported_subcommand_prefixes_cover_named_actions_but_not_other_actions() {
    type PrefixCase<'a> = (&'a [&'a str], &'a [&'a str], &'a [&'a str], &'a str);
    let cases: Vec<PrefixCase<'_>> = vec![
        (
            &["npm", "run", "build"],
            &["npm", "run", "test"],
            &["npm", "install"],
            "exec(npm run *)",
        ),
        (
            &["npm", "exec", "seen"],
            &["npm", "exec", "other"],
            &["npm", "config"],
            "exec(npm exec *)",
        ),
        (
            &["pnpm", "dlx", "seen"],
            &["pnpm", "dlx", "other"],
            &["pnpm", "config"],
            "exec(pnpm dlx *)",
        ),
        (
            &["yarn", "exec", "seen"],
            &["yarn", "exec", "other"],
            &["yarn", "config"],
            "exec(yarn exec *)",
        ),
        (
            &["docker", "run", "alpine"],
            &["docker", "run", "other"],
            &["docker", "build"],
            "exec(docker run *)",
        ),
        (
            &["kubectl", "exec", "pod"],
            &["kubectl", "exec", "other"],
            &["kubectl", "config"],
            "exec(kubectl exec *)",
        ),
        (
            &["go", "run", "./a"],
            &["go", "run", "./b"],
            &["go", "env"],
            "exec(go run *)",
        ),
        (
            &["cargo", "run", "--bin", "a"],
            &["cargo", "run", "--bin", "b"],
            &["cargo", "install"],
            "exec(cargo run *)",
        ),
        (
            &["gh", "alias", "list"],
            &["gh", "alias", "set", "x", "y"],
            &["gh", "config"],
            "exec(gh alias *)",
        ),
        (
            &["git", "remote", "get-url", "origin"],
            &["git", "remote", "set-url", "origin", "other"],
            &["git", "config"],
            "exec(git remote *)",
        ),
        (
            &["git", "update-ref", "refs/heads/a", "HEAD"],
            &["git", "update-ref", "refs/heads/b", "HEAD"],
            &["git", "commit"],
            "exec(git update-ref *)",
        ),
        (
            &["git", "worktree", "list"],
            &["git", "worktree", "add", "other"],
            &["git", "config"],
            "exec(git worktree *)",
        ),
        (
            &["git", "submodule", "status"],
            &["git", "submodule", "update"],
            &["git", "config"],
            "exec(git submodule *)",
        ),
        (
            &["git", "filter-branch", "--tree-filter", "true"],
            &["git", "filter-branch", "--tree-filter", "other"],
            &["git", "config"],
            "exec(git filter-branch *)",
        ),
    ];
    let mut failures = Vec::new();
    for (original, same_action, different_action, expected) in cases {
        let candidates = suggested(&spawn(original));
        let mut rules = Ruleset::default();
        for rule in &candidates {
            Ruleset::merge(&mut rules.allow, rule.clone(), Source::User);
        }
        let engine = PolicyEngine::new(CanonPath::new(WORKSPACE), Mode::Default, rules);
        if candidates
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            != [expected]
            || !matches!(
                engine.classify(spawn(original)),
                Classification::Settled(Ok(_))
            )
            || !matches!(
                engine.classify(spawn(same_action)),
                Classification::Settled(Ok(_))
            )
            || !matches!(
                engine.classify(spawn(different_action)),
                Classification::Ask(_)
            )
        {
            failures.push((original, expected));
        }
    }
    assert!(
        failures.is_empty(),
        "incorrect named-action prefixes: {failures:?}"
    );
}

// Catches: the generic program wildcard reappears, or an ordinary operand is
// mistaken for a subcommand of an unrecognized program.
#[test]
fn ordinary_and_unrecognized_programs_never_gain_generic_prefixes() {
    for (original, expected, different) in [
        (vec!["ls", "src"], "exec(ls src)", vec!["ls", "other"]),
        (
            vec!["custom", "test", "one"],
            "exec(custom test one)",
            vec!["custom", "test", "two"],
        ),
        (vec!["git"], "exec(git)", vec!["git", "status"]),
        (
            vec!["npm", "--version"],
            "exec(npm --version)",
            vec!["npm", "install"],
        ),
        (
            vec!["git", "commit;echo", "one"],
            "exec(git commit;echo one)",
            vec!["git", "commit;echo", "two"],
        ),
    ] {
        assert_eq!(suggested(&spawn(&original))[0].to_string(), expected);
        assert!(
            !covers(&original, &different),
            "{original:?} covers {different:?}"
        );
    }
}

/// Catches an absolute or relative program path, an env assignment, or a
/// launcher being given a prefix rule: the exact form is the only safe one.
#[test]
fn paths_env_assignments_and_launchers_stay_exact() {
    for (remembered, later) in [
        (
            vec!["/usr/bin/python3", "-c", "a"],
            vec!["/usr/bin/python3", "-c", "evil"],
        ),
        (vec!["./tool", "a"], vec!["./tool", "evil"]),
        (
            vec!["FOO=1", "git", "status"],
            vec!["FOO=1", "git", "status", "x"],
        ),
        (
            vec!["env", "git", "status"],
            vec!["env", "sh", "-c", "evil"],
        ),
        (vec!["sudo", "ls"], vec!["sudo", "sh"]),
        (vec!["bash", "-c", "ls"], vec!["bash", "-c", "evil"]),
        (vec!["a*", "x"], vec!["abc", "x"]),
    ] {
        assert!(
            !covers(&remembered, &later),
            "{remembered:?} covers {later:?}"
        );
    }
}

/// Catches `write(<workspace>/**)` reaching the repository's own control
/// files: `.git/hooks/*` runs on the next commit, `.git/config` can name a
/// program, and `.ego` holds the policy that decides the next prompt.
#[test]
fn a_remembered_workspace_write_does_not_reach_git_hooks_or_ego_config() {
    let mut rules = Ruleset::default();
    let candidates = suggested(&write("/workspace/src/a.rs"));
    assert_eq!(
        candidates,
        vec![Rule::load("write(/workspace/**)").unwrap()]
    );
    for rule in candidates {
        Ruleset::merge(&mut rules.allow, rule, Source::User);
    }
    let engine = PolicyEngine::new(CanonPath::new(WORKSPACE), Mode::Default, rules);
    assert!(matches!(
        engine.classify(write("/workspace/src/b.rs")),
        Classification::Settled(Ok(_))
    ));
    let reachable: Vec<&str> = [
        "/workspace/.git/hooks/pre-commit",
        "/workspace/.git/config",
        "/workspace/.ego/config.toml",
        "/workspace/.ego/sandbox/ledger",
        "/workspace/.claude/settings.json",
        "/workspace/vendor/x/.git/hooks/y",
        "/workspace/vendor/x/.ego/config.toml",
        "/workspace/vendor/x/.claude/settings.json",
    ]
    .into_iter()
    .filter(|path| matches!(engine.classify(write(path)), Classification::Settled(Ok(_))))
    .collect();
    assert!(
        reachable.is_empty(),
        "`a` on one write also authorises: {reachable:?}"
    );
}

/// Catches a remembered allow beating a system deny: whatever `a` stored, an
/// administrator's deny for the same operation must still decide it.
#[test]
fn a_system_deny_still_wins_over_every_suggested_rule() {
    use ego_core::config::rule_management::RuleOutcome;

    let mut rules = Ruleset::default();
    Ruleset::merge(
        &mut rules.deny,
        Rule::load("write(/workspace/.git/**)").unwrap(),
        Source::System,
    );
    Ruleset::merge(
        &mut rules.deny,
        Rule::load("exec(git push *)").unwrap(),
        Source::System,
    );
    for rule in suggested(&write("/workspace/src/a.rs"))
        .into_iter()
        .chain(suggested(&spawn(&["git", "push", "origin"])))
    {
        Ruleset::merge(&mut rules.allow, rule, Source::User);
    }
    for call in [
        write("/workspace/.git/hooks/x"),
        spawn(&["git", "push", "origin"]),
    ] {
        let explanation = RuleManagement::explain(&call, &rules, Path::new(WORKSPACE));
        assert_eq!(explanation.outcome, RuleOutcome::Denied);
    }
}
