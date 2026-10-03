//! Critic cases: remembered Git commands must not acquire unseen hook or alias authority.

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

fn assert_changed_command_needs_approval(workspace: &Path, original: &[&str], later: &[&str]) {
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
        matches!(engine.classify(later.clone()), Classification::Ask(_)),
        "remembering {original:?} authorized different control/code argv {later:?}"
    );
    assert_eq!(
        RuleManagement::explain(&later, &rules, workspace).outcome,
        RuleOutcome::Uncovered
    );
}

// Catches: remembering ordinary init authorizes a later template that installs
// unseen hooks, including with a separately located Git control directory.
#[test]
fn remembered_git_init_cannot_authorize_unseen_hook_templates() {
    assert_changed_command_needs_approval(
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

// Catches: remembering clone authorizes installing a different hook template
// into the current repository rather than cloning the approved destination.
#[test]
fn remembered_git_clone_cannot_authorize_unseen_hook_templates_in_workspace() {
    assert_changed_command_needs_approval(
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

// Catches: a Git alias to config evades the exact-argv config exception;
// remembering its query grants a later hooksPath mutation through that alias.
#[test]
fn remembered_unknown_git_alias_cannot_authorize_hook_redirection() {
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
    assert_changed_command_needs_approval(
        &workspace,
        &["git", "inspect", "--get", "user.name"],
        &["git", "inspect", "core.hooksPath", "unseen-hooks"],
    );
}
