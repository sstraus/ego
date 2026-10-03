//! Remembered exec authority: named subcommands cover later arguments; other shapes stay exact.

use std::ffi::OsString;
use std::path::Path;

use ego_core::config::rule_management::{RuleManagement, RuleOutcome};
use ego_core::config::{ManagedRuleEffect, Mode, Ruleset, Source};
use ego_core::permit::{
    CanonPath, Classification, EnvPolicy, Op, PolicyEngine, PreparedCall, SpawnSpec,
};

const WORKSPACE: &str = "/workspace";

fn spawn_in(cwd: &str, argv: &[&str]) -> PreparedCall {
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

fn spawn(argv: &[&str]) -> PreparedCall {
    spawn_in(WORKSPACE, argv)
}

fn covered(rules: &Ruleset, call: &PreparedCall) -> bool {
    let engine = PolicyEngine::new(CanonPath::new(WORKSPACE), Mode::Default, rules.clone());
    let allowed = !matches!(engine.classify(call.clone()), Classification::Ask(_));
    let explained = RuleManagement::explain(call, rules, Path::new(WORKSPACE)).outcome
        != RuleOutcome::Uncovered;
    allowed || explained
}

/// Failures of "remembering `original` must not cover `later`", one line each.
fn widened(cases: &[(&[&str], &[&str])]) -> Vec<String> {
    let mut failures = Vec::new();
    for (original, later) in cases {
        let mut rules = Ruleset::default();
        let suggestion = RuleManagement::suggest_remember(
            &spawn(original),
            ManagedRuleEffect::Allow,
            Path::new(WORKSPACE),
        )
        .unwrap();
        let mut stored = Vec::new();
        for candidate in suggestion.candidates {
            stored.push(candidate.to_string());
            Ruleset::merge(&mut rules.allow, candidate, Source::User);
        }
        assert!(
            covered(&rules, &spawn(original)),
            "remembered rule {stored:?} no longer covers the approved {original:?}"
        );
        if covered(&rules, &spawn(later)) {
            failures.push(format!(
                "{original:?} stored {stored:?} which covers {later:?}"
            ));
        }
    }
    failures
}

fn assert_none_widened(cases: &[(&[&str], &[&str])]) {
    let failures = widened(cases);
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

fn assert_all_covered(cases: &[(&[&str], &[&str])]) {
    for (original, later) in cases {
        let mut rules = Ruleset::default();
        for candidate in RuleManagement::suggest_remember(
            &spawn(original),
            ManagedRuleEffect::Allow,
            Path::new(WORKSPACE),
        )
        .unwrap()
        .candidates
        {
            Ruleset::merge(&mut rules.allow, candidate, Source::User);
        }
        let engine = PolicyEngine::new(CanonPath::new(WORKSPACE), Mode::Default, rules.clone());
        for argv in [original, later] {
            let call = spawn(argv);
            assert!(
                matches!(
                    engine.classify(call.clone()),
                    Classification::Settled(Ok(_))
                ),
                "remembering {original:?} must cover {argv:?}"
            );
            assert_eq!(
                RuleManagement::explain(&call, &rules, Path::new(WORKSPACE)).outcome,
                RuleOutcome::Allowed,
                "remembering {original:?} must explain {argv:?} as allowed"
            );
        }
    }
}

// Catches: launcher/Git recognition is case-sensitive while the filesystem
// resolving the program is not (APFS, NTFS): remembering `Bash -c x` stores
// `exec(Bash *)`, which covers any later code run through the same shell.
#[test]
fn remembered_case_variants_of_launchers_and_git_keep_exact_argv() {
    assert_none_widened(&[
        (&["Bash", "-c", "echo seen"], &["Bash", "-c", "echo unseen"]),
        (&["SH", "-c", "echo seen"], &["SH", "-c", "echo unseen"]),
        (
            &["Python3", "-c", "print(1)"],
            &["Python3", "-c", "print(2)"],
        ),
        (&["NODE", "-e", "1"], &["NODE", "-e", "2"]),
        (&["Env", "ls"], &["Env", "sh", "-c", "unseen"]),
        (&["Sudo", "ls"], &["Sudo", "sh", "-c", "unseen"]),
        (
            &["Git", "status"],
            &["Git", "config", "core.hooksPath", "unseen"],
        ),
        (
            &["GIT", "log"],
            &["GIT", "init", "--template=/workspace/unseen"],
        ),
    ]);
}

// Catches: the interpreter list names only `python3`/`node`; a versioned or
// platform-suffixed interpreter, or an unlisted runner, receives `exec(<p> *)`.
#[test]
fn remembered_unlisted_interpreters_and_runners_keep_exact_argv() {
    assert_none_widened(&[
        (&["python", "-c", "print(1)"], &["python", "-c", "print(2)"]),
        (
            &["python3.12", "-c", "print(1)"],
            &["python3.12", "-c", "print(2)"],
        ),
        (&["python2", "-c", "1"], &["python2", "-c", "2"]),
        (&["nodejs", "-e", "1"], &["nodejs", "-e", "2"]),
        (&["bash.exe", "-c", "1"], &["bash.exe", "-c", "2"]),
        (&["pwsh", "-c", "1"], &["pwsh", "-c", "2"]),
        (
            &["powershell", "-Command", "1"],
            &["powershell", "-Command", "2"],
        ),
        (&["cmd", "/c", "dir"], &["cmd", "/c", "del"]),
        (&["tsx", "a.ts"], &["tsx", "-e", "unseen"]),
        (&["bunx", "a"], &["bunx", "b"]),
        (
            &["uv", "run", "a.py"],
            &["uv", "run", "--with", "evil", "b.py"],
        ),
        (&["pipx", "run", "a"], &["pipx", "run", "b"]),
        (&["caffeinate", "ls"], &["caffeinate", "sh", "-c", "unseen"]),
        (
            &["arch", "-arm64", "ls"],
            &["arch", "-arm64", "sh", "-c", "unseen"],
        ),
    ]);
}

// Catches: treating config queries as exact-only despite the displayed
// named-subcommand prefix, or disagreeing between admission and explanation.
#[test]
fn remembered_tool_subcommand_prefixes_cover_config_writes() {
    assert_all_covered(&[
        (
            &["npm", "config", "get", "registry"],
            &["npm", "config", "set", "script-shell", "/workspace/evil"],
        ),
        (
            &["yarn", "config", "get", "x"],
            &["yarn", "config", "set", "yarnPath", "/workspace/evil"],
        ),
        (
            &["pnpm", "config", "get", "x"],
            &["pnpm", "config", "set", "script-shell", "/workspace/evil"],
        ),
        (
            &["gh", "alias", "list"],
            &["gh", "alias", "set", "--shell", "x", "sh -c unseen"],
        ),
        (
            &["gh", "config", "get", "editor"],
            &["gh", "config", "set", "editor", "/workspace/evil"],
        ),
        (
            &["go", "env", "GOFLAGS"],
            &["go", "env", "-w", "GOFLAGS=-toolexec=/workspace/evil"],
        ),
        (
            &["kubectl", "config", "view"],
            &[
                "kubectl",
                "config",
                "set-credentials",
                "u",
                "--exec-command=/workspace/evil",
            ],
        ),
    ]);
}

// Catches: keeping a launcher exception list that narrows a displayed
// named-subcommand prefix instead of covering its later program arguments.
#[test]
fn remembered_code_running_subcommand_prefixes_cover_later_programs() {
    assert_all_covered(&[
        (
            &["yarn", "exec", "ls"],
            &["yarn", "exec", "sh", "-c", "unseen"],
        ),
        (&["npm", "init", "foo"], &["npm", "init", "bar"]),
        (&["npm", "create", "foo"], &["npm", "create", "bar"]),
        (
            &["npm", "explore", "pkg", "--", "ls"],
            &["npm", "explore", "pkg", "--", "sh", "-c", "unseen"],
        ),
        (&["pnpm", "create", "foo"], &["pnpm", "create", "bar"]),
        (&["yarn", "create", "foo"], &["yarn", "create", "bar"]),
        (
            &["docker", "container", "exec", "c", "ls"],
            &["docker", "container", "exec", "c", "sh", "-c", "unseen"],
        ),
        (
            &["docker", "compose", "run", "s", "ls"],
            &["docker", "compose", "run", "s", "sh", "-c", "unseen"],
        ),
    ]);
}

// Catches: filtering runner/config options after an approved subcommand,
// or treating a leading option as a subcommand and widening exact argv.
#[test]
fn remembered_build_prefixes_cover_program_options_but_leading_options_stay_exact() {
    assert_all_covered(&[
        (
            &["go", "test", "./..."],
            &["go", "test", "-exec", "/workspace/evil", "./..."],
        ),
        (
            &["go", "build", "./..."],
            &["go", "build", "-toolexec", "/workspace/evil", "./..."],
        ),
        (
            &["cargo", "test"],
            &[
                "cargo",
                "test",
                "--config",
                "target.x.runner=\"/workspace/evil\"",
            ],
        ),
        (
            &["cargo", "build"],
            &[
                "cargo",
                "build",
                "--config",
                "build.rustc-wrapper=\"/workspace/evil\"",
            ],
        ),
        (
            &["npm", "run", "build"],
            &["npm", "run", "build", "--script-shell=/workspace/evil"],
        ),
    ]);
    assert_none_widened(&[
        (
            &[
                "cargo",
                "--config",
                "target.x.runner=\"/workspace/seen\"",
                "test",
            ],
            &[
                "cargo",
                "--config",
                "target.x.runner=\"/workspace/evil\"",
                "test",
            ],
        ),
        (
            &["git", "-c", "core.hooksPath=seen", "status"],
            &["git", "-c", "core.hooksPath=unseen", "status"],
        ),
    ]);
}

// Catches: retaining the retired ls-tree allowlist instead of covering plain
// subcommand spellings, or treating an unrecognized executable as git.
#[test]
fn remembered_git_plain_subcommand_spellings_get_prefixes_but_git_exe_stays_exact() {
    assert_all_covered(&[
        (&["git", "LS-TREE", "HEAD"], &["git", "LS-TREE", "--x"]),
        (&["git", "ls-tree.x", "HEAD"], &["git", "ls-tree.x", "--x"]),
        (&["git", "ls_tree", "HEAD"], &["git", "ls_tree", "--x"]),
        (&["git", "ls-tree+", "HEAD"], &["git", "ls-tree+", "--x"]),
    ]);
    assert_none_widened(&[(
        &["git.exe", "log"],
        &["git.exe", "config", "core.hooksPath", "x"],
    )]);
}
