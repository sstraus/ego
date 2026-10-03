//! Critic regressions for regression case: actual filesystem aliases and remembered exec widening.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ego_core::config::rule_management::{RuleManagement, RuleOutcome};
use ego_core::config::{ManagedRuleEffect, Mode, Ruleset, Source};
use ego_core::permit::{
    CanonPath, Change, Classification, EnvPolicy, Op, PolicyEngine, PreparedCall, SpawnSpec,
};

struct Repository(PathBuf);

impl Repository {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ego-regression-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let output = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git init: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self(std::fs::canonicalize(root).unwrap())
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn write(path: PathBuf) -> PreparedCall {
    PreparedCall::new(
        "write",
        Op::Write {
            path: CanonPath::new(path),
            change: Change::Whole(b"changed".to_vec()),
        },
    )
}

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

fn remembered(call: &PreparedCall, workspace: &Path) -> Ruleset {
    let mut rules = Ruleset::default();
    for rule in RuleManagement::suggest_remember(call, ManagedRuleEffect::Allow, workspace)
        .unwrap()
        .candidates
    {
        Ruleset::merge(&mut rules.allow, rule, Source::User);
    }
    rules
}

// Catches: APFS returns the stored .GIT/.Git spelling after canonicalization,
// allowing remembered workspace writes to modify the same Git control directory.
#[cfg(target_os = "macos")]
#[test]
fn policy_control_case_aliases_cannot_gain_allow_authority_on_apfs() {
    let repository = Repository::new();
    let rules = remembered(&write(repository.0.join("ordinary.txt")), &repository.0);
    let engine = PolicyEngine::new(CanonPath::new(&repository.0), Mode::Default, rules.clone());
    let mut covered = Vec::new();
    for variant in [".GIT", ".Git"] {
        std::fs::rename(
            repository.0.join(".git"),
            repository.0.join("control-renaming"),
        )
        .unwrap();
        std::fs::rename(
            repository.0.join("control-renaming"),
            repository.0.join(variant),
        )
        .unwrap();
        let canonical = std::fs::canonicalize(repository.0.join(".git/config")).unwrap();
        assert!(
            canonical.starts_with(repository.0.join(variant)),
            "live filesystem must retain stored spelling: {canonical:?}"
        );
        let call = write(canonical);
        if !matches!(engine.classify(call.clone()), Classification::Ask(_))
            || RuleManagement::explain(&call, &rules, &repository.0).outcome
                != RuleOutcome::Uncovered
        {
            covered.push(variant);
        }
        std::fs::rename(
            repository.0.join(variant),
            repository.0.join("control-renaming"),
        )
        .unwrap();
        std::fs::rename(
            repository.0.join("control-renaming"),
            repository.0.join(".git"),
        )
        .unwrap();
    }
    assert!(
        covered.is_empty(),
        "control directory aliases gained allow authority: {covered:?}"
    );
}

// Catches: an approved read-only git config query becomes a prefix that also
// permits changing core.hooksPath, authorizing unseen code on a later commit.
#[test]
fn policy_control_remembered_git_config_query_cannot_authorize_hook_redirection() {
    let workspace = Path::new("/workspace");
    let original = spawn(workspace, &["git", "config", "--get", "user.name"]);
    let rules = remembered(&original, workspace);
    let later = spawn(
        workspace,
        &[
            "git",
            "config",
            "--local",
            "core.hooksPath",
            "/workspace/hooks",
        ],
    );
    let engine = PolicyEngine::new(CanonPath::new(workspace), Mode::Default, rules.clone());
    assert!(
        matches!(engine.classify(later.clone()), Classification::Ask(_)),
        "remembered git config query authorized a control configuration write"
    );
    assert_eq!(
        RuleManagement::explain(&later, &rules, workspace).outcome,
        RuleOutcome::Uncovered
    );
}

// Catches: checking the symlink's ordinary name rather than the canonical
// target lets a remembered write reach .git/config through an alias.
#[cfg(unix)]
#[test]
fn policy_control_symlink_and_dot_components_resolve_to_uncovered_git_config() {
    let repository = Repository::new();
    std::os::unix::fs::symlink(repository.0.join(".git"), repository.0.join("ordinary")).unwrap();
    std::fs::create_dir(repository.0.join("nested")).unwrap();
    let rules = remembered(&write(repository.0.join("ordinary.txt")), &repository.0);
    let engine = PolicyEngine::new(CanonPath::new(&repository.0), Mode::Default, rules.clone());
    for input in [
        "ordinary/config",
        "nested/../ordinary/./config",
        ".git/./config",
    ] {
        let canonical = std::fs::canonicalize(repository.0.join(input)).unwrap();
        let call = write(canonical);
        assert!(
            matches!(engine.classify(call.clone()), Classification::Ask(_)),
            "{input}"
        );
        assert_eq!(
            RuleManagement::explain(&call, &rules, &repository.0).outcome,
            RuleOutcome::Uncovered,
            "{input}"
        );
    }
}
