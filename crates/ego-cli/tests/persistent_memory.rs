//! Contracts for operator-only persistent state; no hosted calls or MDKB writes.
use ego_cli::memory::{InitProposal, Operation, Request, init_admitted};
use ego_cli::repl::actions::{
    ActionId, ActionKind, ActionRegistry, Availability, Dispatch, InputOrigin,
};
use ego_core::config::{ConfigLayer, Source, Trust, resolve};

#[test]
fn memory_and_init_cannot_fall_through_to_model_or_bypass_disabled_actions() {
    // Catches: command bodies become paid model prompts or bypass a disabled snapshot.
    let registry = ActionRegistry::final_surface();
    for (name, id, expected) in [
        ("memory", ActionId::Memory, Dispatch::Memory(String::new())),
        ("init", ActionId::Init, Dispatch::Init(String::new())),
    ] {
        assert_eq!(registry.kind(id), ActionKind::Local);
        assert_eq!(
            registry.classify(registry.submit(format!("/{name}"), InputOrigin::Typed)),
            expected
        );
        assert_eq!(
            registry.classify(registry.submit(format!("/{name}"), InputOrigin::BracketedPaste)),
            Dispatch::Prompt
        );
        let disabled = ActionRegistry::final_surface().with_availability(id, Availability::Busy);
        assert!(matches!(
            disabled.classify(disabled.submit(format!("/{name}"), InputOrigin::Typed)),
            Dispatch::Refused { .. }
        ));
    }
    assert_eq!(
        registry.classify(registry.submit("/memory user show decision", InputOrigin::Typed)),
        Dispatch::Memory("user show decision".into())
    );
}

#[test]
fn memory_scope_and_operator_text_parse_without_shell_expansion() {
    // Catches: user scope leaks into project scope or quoted content becomes executable syntax.
    let request = Request::parse("user set lesson 'A title' 'Never run $(tool)' ").unwrap();
    assert!(request.user);
    assert_eq!(
        request.operation,
        Operation::Set {
            id: "lesson".into(),
            title: "A title".into(),
            content: "Never run $(tool)".into()
        }
    );
    assert!(!Request::parse("project list").unwrap().user);
    for input in [
        "delete --help",
        "show ../other",
        "set id title",
        "set id '' content",
        "disable id extra",
    ] {
        assert!(Request::parse(input).is_err(), "{input}");
    }
}

#[test]
fn new_workspace_initialization_is_a_reviewed_diff_not_an_implicit_write() {
    // Catches: proposal or declined confirmation creates ambient instructions.
    let dir = tempfile::tempdir().unwrap();
    let proposal = InitProposal::prepare(dir.path()).unwrap();
    assert!(proposal.diff().contains("+++ AGENTS.md"));
    assert!(proposal.diff().contains("CLAUDE.md (relative symlink)"));
    assert!(!dir.path().join("AGENTS.md").exists());
    assert!(
        proposal
            .apply(true, &|preview| {
                assert!(preview.contains("+AGENTS.md"));
                false
            })
            .unwrap()
            .contains("declined")
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    proposal.apply(true, &|_| true).unwrap();
    assert!(
        std::fs::read_to_string(dir.path().join("AGENTS.md"))
            .unwrap()
            .contains("# Project instructions")
    );
    assert_eq!(
        std::fs::read_link(dir.path().join("CLAUDE.md")).unwrap(),
        std::path::Path::new("AGENTS.md")
    );
}

#[test]
fn existing_instructions_are_preserved_and_independent_claude_is_refused() {
    // Catches: bootstrap overwrites project-specific instructions or discards unique CLAUDE content.
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("AGENTS.md");
    std::fs::write(&agents, "# Existing\nUnique project rule.\n").unwrap();
    let proposal = InitProposal::prepare(dir.path()).unwrap();
    assert!(!proposal.diff().contains("+++ AGENTS.md"));
    proposal.apply(true, &|_| true).unwrap();
    assert_eq!(
        std::fs::read_to_string(&agents).unwrap(),
        "# Existing\nUnique project rule.\n"
    );
    let existing = InitProposal::prepare(dir.path()).unwrap();
    assert!(
        existing
            .apply(true, &|_| panic!("no-op must not ask"))
            .unwrap()
            .contains("already exist")
    );
    std::fs::remove_file(dir.path().join("CLAUDE.md")).unwrap();
    std::fs::write(dir.path().join("CLAUDE.md"), "Independent rules").unwrap();
    assert!(
        InitProposal::prepare(dir.path())
            .unwrap_err()
            .contains("reconcile")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
        "Independent rules"
    );
}

#[test]
fn denied_writes_and_post_review_changes_leave_files_untouched() {
    // Catches: denial or a stale reviewed proposal overwrites an instruction file.
    let dir = tempfile::tempdir().unwrap();
    let proposal = InitProposal::prepare(dir.path()).unwrap();
    assert!(
        proposal
            .apply(false, &|_| panic!("denied writes must not ask"))
            .is_err()
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    let result = proposal.apply(true, &|_| {
        std::fs::write(dir.path().join("AGENTS.md"), "Concurrent owner").unwrap();
        true
    });
    assert!(result.unwrap_err().contains("changed after review"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
        "Concurrent owner"
    );
    assert!(!dir.path().join("CLAUDE.md").exists());
}

#[test]
fn init_respects_plan_read_only_and_explicit_deny_before_confirmation() {
    // Catches: an explicit operator command bypasses the effective write floor.
    let dir = tempfile::tempdir().unwrap();
    for config in [
        "mode = 'plan'",
        "sandbox = 'ro'",
        "[permissions]\ndeny = ['write:**']",
    ] {
        let layer = ConfigLayer::parse(Source::User, config).unwrap();
        let effective = resolve(&[layer], Trust::Trusted, None).unwrap();
        assert!(
            !init_admitted(&effective, dir.path(), Trust::Trusted),
            "{config}"
        );
    }
    let effective = resolve(&[], Trust::Untrusted, None).unwrap();
    assert!(!init_admitted(&effective, dir.path(), Trust::Untrusted));
}

#[tokio::test]
async fn untrusted_memory_never_reads_or_writes_the_native_store() {
    // Catches: untrusted workspace content starts an ambient MDKB process.
    let dir = tempfile::tempdir().unwrap();
    let effective = resolve(&[], Trust::Untrusted, None).unwrap();
    let result = ego_cli::memory::command(
        "set note title content",
        dir.path(),
        None,
        &effective,
        Trust::Untrusted,
        &|_| panic!("untrusted must not ask"),
    )
    .await;
    assert!(result.unwrap_err().contains("trusted workspace"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn declining_memory_review_does_not_invoke_native_persistence() {
    // Catches: native persistence happens before the operator sees the exact body.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".mdkb")).unwrap();
    let layer = ConfigLayer::parse(Source::User, "sandbox = 'off'").unwrap();
    let effective = resolve(&[layer], Trust::Trusted, None).unwrap();
    let result = ego_cli::memory::command(
        "set note title 'Operator text'",
        dir.path(),
        None,
        &effective,
        Trust::Trusted,
        &|preview| {
            assert!(preview.contains("Operator text"));
            assert!(preview.contains("user_statement"));
            assert!(preview.contains("Do not save secrets"));
            false
        },
    )
    .await
    .unwrap();
    assert!(result.contains("declined"));
    assert_eq!(
        std::fs::read_dir(dir.path().join(".mdkb")).unwrap().count(),
        0
    );
}
