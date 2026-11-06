//! Remembered Git subcommand prefixes cover later templates and alias arguments.

use std::ffi::OsString;
use std::path::Path;

use ego_core::config::rule_management::{RuleManagement, RuleOutcome};
use ego_core::config::{ManagedRuleEffect, Mode, Ruleset, Source};
use ego_core::permit::{
    CanonPath, Classification, EnvPolicy, Op, PolicyEngine, PreparedCall, SpawnSpec,
};

fn spawn(workspace: &Path, argv: &[&str]) -> PreparedCall {
    PreparedCall::new(
        "exec",
        Op::Spawn {
            spec: SpawnSpec::new(
                argv.iter().map(OsString::from).collect(),
                Some(CanonPath::new(workspace)),
                None,
                EnvPolicy::exact(Vec::new()).unwrap(),
            )
            .unwrap(),
        },
    )
}

fn assert_changed_command_is_covered(workspace: &Path, original: &[&str], later: &[&str]) {
    let original = spawn(workspace, original);
    let mut rules = Ruleset::default();
    for candidate in
        RuleManagement::suggest_remember(&original, ManagedRuleEffect::Allow, workspace)
            .unwrap()
            .candidates
    {
        Ruleset::merge(&mut rules.allow, candidate, Source::User);
    }
    let later = spawn(workspace, later);
    let engine = PolicyEngine::new(CanonPath::new(workspace), Mode::Default, rules.clone());
    assert!(
        matches!(
            engine.classify(later.clone()),
            Classification::Settled(Ok(_))
        ),
        "remembering {original:?} must cover later arguments {later:?}"
    );
    assert_eq!(
        RuleManagement::explain(&later, &rules, workspace).outcome,
        RuleOutcome::Allowed
    );
}

// Catches: retaining an exact-only init exception that rejects template or
// control-directory arguments covered by the displayed git init prefix.
#[test]
fn remembered_git_init_prefix_covers_later_hook_templates() {
    assert_changed_command_is_covered(
        Path::new("/workspace"),
        &["git", "init", "."],
        &[
            "git",
            "init",
            "--template=/workspace/unseen-template",
            "--separate-git-dir=/workspace/control",
            ".",
        ],
    );
}

// Catches: retaining an exact-only clone exception that rejects a later
// template or destination covered by the displayed git clone prefix.
#[test]
fn remembered_git_clone_prefix_covers_later_hook_templates_in_workspace() {
    assert_changed_command_is_covered(
        Path::new("/workspace"),
        &["git", "clone", "/workspace/source", "ordinary"],
        &[
            "git",
            "clone",
            "--template=/workspace/unseen-template",
            "/workspace/source",
            ".",
        ],
    );
}

// Catches: narrowing a plain Git alias to exact argv despite the displayed
// alias prefix; the live Git probe records the alias configuration effect.
#[test]
fn remembered_git_alias_prefix_covers_later_hook_redirection() {
    let workspace = std::env::temp_dir().join(format!("ego-critic-alias-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&workspace).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(&workspace)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    git(&["init", "--quiet"]);
    git(&["config", "user.name", "Critic"]);
    git(&["config", "alias.inspect", "!git config"]);
    assert_eq!(git(&["inspect", "--get", "user.name"]), b"Critic\n");
    git(&["inspect", "core.hooksPath", "unseen-hooks"]);
    assert_eq!(
        git(&["config", "--get", "core.hooksPath"]),
        b"unseen-hooks\n"
    );
    std::fs::remove_dir_all(&workspace).unwrap();
    assert_changed_command_is_covered(
        &workspace,
        &["git", "inspect", "--get", "user.name"],
        &["git", "inspect", "core.hooksPath", "unseen-hooks"],
    );
}
