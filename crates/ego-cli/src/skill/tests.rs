//! Contract tests for shared YAML extraction and skill admission.
//!
//! The service used here is deliberately the CLI-owned filesystem adapter. It
//! must reuse the bounded envelope used by agent files while leaving policy,
//! source authority, and immutable catalogue facts in their existing owners.

use std::fs;
use std::path::{Path, PathBuf};

use super::{SkillCatalogue, SkillCatalogueCapture, SkillDiscovery, SkillSourceAuthority};
use ego_core::config::{ConfigLayer, Source, Trust};
use tempfile::TempDir;

const MAX_FILE_BYTES: usize = 1024 * 1024;

struct Roots {
    _dir: TempDir,
    system: PathBuf,
    user: PathBuf,
    project: PathBuf,
}

impl Roots {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("skill roots");
        let system = dir.path().join("etc/ego/skills");
        let user = dir.path().join("home/.ego/skills");
        let project = dir.path().join("workspace/.ego/skills");
        for root in [&system, &user, &project] {
            fs::create_dir_all(root).expect("create skill root");
        }
        Self {
            _dir: dir,
            system,
            user,
            project,
        }
    }

    fn write(&self, root: &Path, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        let directory = root.join(name);
        fs::create_dir_all(&directory).expect("create skill directory");
        let path = directory.join("SKILL.md");
        fs::write(&path, format!("---\n{frontmatter}\n---\n{body}\n")).expect("write skill");
        path
    }

    fn discovery(&self, trusted_project: bool) -> SkillDiscovery {
        SkillDiscovery::new(
            self.system.clone(),
            self.user.clone(),
            self.project.clone(),
            trusted_project,
        )
    }

    fn catalogue(&self, trusted_project: bool) -> SkillCatalogue {
        SkillCatalogue::discover(self.discovery(trusted_project), &["help", "skills", "run"])
            .expect("valid skill catalogue")
    }
}

fn capture(catalogue: &SkillCatalogue) -> SkillCatalogueCapture {
    catalogue.capture().expect("skill catalogue capture")
}

fn valid_for(name: &str, frontmatter: &str) -> String {
    format!("name: {name}\ndescription: Explain a failing check.\n{frontmatter}")
}

#[test]
fn shared_yaml_envelope_projects_invocation_skill_hooks_into_typed_core_entries() {
    let text = r#"---
version: 1
hooks:
  - event: SubagentStop
    matcher: "read(src/**)"
    once: true
    handler:
      type: command
      command: /usr/bin/skill-hook
      args: []
---
The skill body is not part of hook configuration.
"#;
    let (header, body) = crate::yaml_envelope::split(text, false).expect("shared envelope");
    assert!(body.contains("not part of hook configuration"));
    let invocation = ego_core::event::skill::SkillInvocationId::new();
    let entries = super::project_skill_hooks(header, "skills/demo/SKILL.md", invocation)
        .expect("typed skill hook projection");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].event(), "SubagentStop");
    assert!(entries[0].once());
    assert_eq!(entries[0].source_location(), "skills/demo/SKILL.md");
    assert_eq!(entries[0].invocation(), invocation);
}

mod regression_case {
    use super::*;

    #[test]
    fn agent_file_and_skill_share_the_bounded_envelope_without_changing_agent_shape() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "explain-failure",
            &valid_for("explain-failure", "allowed-tools: \"read grep\""),
            "Read direct evidence.",
        );
        let catalogue = roots.catalogue(false);
        let captured = capture(&catalogue);
        let descriptor = captured
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.name() == "explain-failure")
            .expect("skill descriptor");
        assert_eq!(descriptor.source(), SkillSourceAuthority::User);
        assert!(!format!("{descriptor:?}").contains("Read direct evidence."));
        assert!(!format!("{descriptor:?}").contains("grep"));
    }

    #[test]
    fn discovery_assigns_canonical_source_authority_and_does_not_scan_claude_aliases() {
        let roots = Roots::new();
        roots.write(
            &roots.system,
            "system-skill",
            &valid_for("system-skill", ""),
            "system",
        );
        roots.write(
            &roots.user,
            "user-skill",
            &valid_for("user-skill", ""),
            "user",
        );
        roots.write(
            &roots.project,
            "project-skill",
            &valid_for("project-skill", ""),
            "project",
        );
        let claude = roots
            ._dir
            .path()
            .join("workspace/.claude/skills/alias/SKILL.md");
        fs::create_dir_all(claude.parent().unwrap()).unwrap();
        fs::write(
            &claude,
            "---\nname: alias\ndescription: alias\n---\nalias\n",
        )
        .unwrap();

        let catalogue = roots.catalogue(true);
        let captured = capture(&catalogue);
        let mut sources = captured
            .descriptors()
            .iter()
            .map(|descriptor| (descriptor.name(), descriptor.source()))
            .collect::<Vec<_>>();
        sources.sort_by_key(|(name, _)| *name);
        assert_eq!(
            sources,
            vec![
                ("context-management", SkillSourceAuthority::System),
                ("project-skill", SkillSourceAuthority::Project),
                ("system-skill", SkillSourceAuthority::System),
                ("user-skill", SkillSourceAuthority::User),
            ]
        );
        assert!(captured.diagnostics().iter().all(|diagnostic| {
            !diagnostic
                .source_path()
                .to_string_lossy()
                .contains(".claude")
        }));
    }

    #[test]
    fn canonical_allowed_tools_scalar_is_admitted_and_noncanonical_shapes_refuse_by_name() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "canonical",
            &valid_for("canonical", "allowed-tools: \"read grep\""),
            "body",
        );
        let catalogue = roots.catalogue(false);
        assert!(
            capture(&catalogue)
                .descriptors()
                .iter()
                .any(|descriptor| descriptor.name() == "canonical")
        );

        for (name, field) in [
            ("list-form", "allowed-tools:\n  - read\n  - grep"),
            ("comma-form", "allowed-tools: \"read,grep\""),
            (
                "unknown-form",
                "allowed-tools: \"read definitely-not-a-tool\"",
            ),
            ("empty-form", "allowed-tools: \"\""),
            ("deprecated-form", "allowed_tools: \"read\""),
        ] {
            roots.write(&roots.user, name, &valid_for(name, field), "body");
        }
        let candidate = SkillCatalogue::discover(roots.discovery(false), &["read", "grep"])
            .expect("candidate diagnostics are non-fatal");
        let captured = capture(&candidate);
        for name in [
            "list-form",
            "comma-form",
            "unknown-form",
            "empty-form",
            "deprecated-form",
        ] {
            let diagnostic = captured
                .diagnostics()
                .iter()
                .find(|diagnostic| diagnostic.name() == name)
                .expect("exact rejected skill diagnostic");
            assert!(diagnostic.message().contains("allowed-tools"));
        }
    }

    #[test]
    fn precedence_selects_user_over_system_but_fixed_actions_remain_reserved() {
        let roots = Roots::new();
        roots.write(
            &roots.system,
            "duplicate",
            &valid_for("duplicate", ""),
            "system",
        );
        roots.write(
            &roots.user,
            "duplicate",
            &valid_for("duplicate", ""),
            "user",
        );
        roots.write(
            &roots.user,
            "skills",
            &valid_for("skills", ""),
            "must not replace fixed action",
        );
        let catalogue = SkillCatalogue::discover(roots.discovery(false), &["skills", "help"])
            .expect("collisions are projected diagnostics");
        let captured = capture(&catalogue);
        assert_eq!(
            captured.descriptor("duplicate").unwrap().source(),
            SkillSourceAuthority::User
        );
        assert!(captured.descriptor("skills").is_none());
        assert!(
            captured
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.name() == "skills" && diagnostic.is_collision())
        );
    }

    #[test]
    fn untrusted_project_skills_are_not_read_or_admitted() {
        let roots = Roots::new();
        roots.write(
            &roots.project,
            "project-only",
            &valid_for("project-only", ""),
            "secret project body",
        );
        let catalogue = roots.catalogue(false);
        assert!(
            capture(&catalogue)
                .descriptors()
                .iter()
                .all(|descriptor| descriptor.name() != "project-only")
        );
        assert!(capture(&catalogue).diagnostics().is_empty());
    }

    #[test]
    fn invalid_reload_retains_last_good_immutable_catalogue_and_exposes_exact_diagnostic() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "stable",
            &valid_for("stable", ""),
            "first body",
        );
        let catalogue = roots.catalogue(false);
        let before = capture(&catalogue);
        roots.write(
            &roots.user,
            "new-valid",
            &valid_for("new-valid", ""),
            "valid body",
        );
        roots.write(
            &roots.user,
            "stable",
            "name: stable\nunknown: true",
            "bad body",
        );
        roots.write(&roots.user, "new-invalid", "name: new-invalid", "bad body");

        let prepared = catalogue
            .prepare_rescan()
            .expect("prepare returns last good prospective capture");
        let prospective = prepared.capture();
        assert!(prospective.retained_last_good());
        assert!(std::sync::Arc::ptr_eq(&prospective.core(), &before.core()));
        assert_eq!(prospective.snapshot_digest(), before.snapshot_digest());
        assert!(capture(&catalogue).diagnostics().is_empty());
        let committed = catalogue
            .commit_rescan(prepared)
            .expect("commit publishes the retained last-good capture");
        assert!(committed.retained_last_good());
        assert!(std::sync::Arc::ptr_eq(&committed.core(), &before.core()));
        assert_eq!(committed.snapshot_digest(), before.snapshot_digest());
        assert!(
            committed
                .descriptors()
                .iter()
                .any(|descriptor| descriptor.name() == "stable")
        );
        assert!(
            committed
                .descriptors()
                .iter()
                .all(|descriptor| descriptor.name() != "new-invalid")
        );
        assert!(
            committed
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.name() == "stable"
                    && diagnostic.message().contains("unknown"))
        );
    }

    #[test]
    fn prepared_valid_rescan_is_viewless_until_atomic_commit_and_supplies_new_core() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "stable",
            &valid_for("stable", ""),
            "first body",
        );
        let catalogue = roots.catalogue(false);
        let before = capture(&catalogue);
        roots.write(
            &roots.user,
            "new-valid",
            &valid_for("new-valid", "allowed-tools: \"read\""),
            "valid body",
        );

        let prepared = catalogue
            .prepare_rescan()
            .expect("valid candidate is prepared");
        let prospective = prepared.capture();
        assert!(!prospective.retained_last_good());
        assert!(
            prospective
                .descriptors()
                .iter()
                .any(|descriptor| descriptor.name() == "new-valid")
        );
        assert!(std::sync::Arc::ptr_eq(
            &capture(&catalogue).core(),
            &before.core()
        ));
        assert_eq!(
            capture(&catalogue).snapshot_digest(),
            before.snapshot_digest()
        );

        // The preflight consumer receives the exact prospective core. It is
        // intentionally retained here as the value that model-tool assembly
        // must consume before publication.
        let preflight_core = prospective.core();
        assert!(
            preflight_core
                .specs()
                .iter()
                .any(|descriptor| descriptor.name() == "new-valid")
        );
        let committed = catalogue
            .commit_rescan(prepared)
            .expect("valid candidate commits atomically");
        assert!(!committed.retained_last_good());
        assert!(std::sync::Arc::ptr_eq(&committed.core(), &preflight_core));
        assert_ne!(committed.snapshot_digest(), before.snapshot_digest());
        assert!(
            capture(&catalogue)
                .descriptors()
                .iter()
                .any(|descriptor| descriptor.name() == "new-valid")
        );
    }

    #[test]
    fn stale_prepared_rescan_refuses_without_overwriting_newer_publication() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "stable",
            &valid_for("stable", ""),
            "first body",
        );
        let catalogue = roots.catalogue(false);
        roots.write(
            &roots.user,
            "first",
            &valid_for("first", ""),
            "first candidate",
        );
        let stale = catalogue
            .prepare_rescan()
            .expect("first candidate is prepared");
        roots.write(
            &roots.user,
            "second",
            &valid_for("second", ""),
            "second candidate",
        );
        let latest = catalogue
            .prepare_rescan()
            .expect("second candidate is prepared from the same base");
        let latest_capture = catalogue
            .commit_rescan(latest)
            .expect("latest candidate commits");
        assert!(
            latest_capture
                .descriptors()
                .iter()
                .any(|descriptor| descriptor.name() == "second")
        );
        let before_stale_commit = capture(&catalogue);
        assert!(catalogue.commit_rescan(stale).is_err());
        let after_stale_commit = capture(&catalogue);
        assert!(std::sync::Arc::ptr_eq(
            &before_stale_commit.core(),
            &after_stale_commit.core()
        ));
        assert_eq!(
            before_stale_commit.snapshot_digest(),
            after_stale_commit.snapshot_digest()
        );
    }

    #[test]
    fn profile_mapping_and_final_owner_gates_are_exact() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "forked",
            &valid_for("forked", "context: fork\nagent: trusted\nbackground: false"),
            "body",
        );
        roots.write(
            &roots.user,
            "agent-without-fork",
            &valid_for("agent-without-fork", "agent: trusted"),
            "body",
        );
        roots.write(
            &roots.user,
            "temporary",
            &valid_for("temporary", "context: fork\nshell: bash"),
            "body",
        );
        roots.write(
            &roots.user,
            "hooked",
            &valid_for("hooked", "hooks: []"),
            "body",
        );
        let catalogue = roots.catalogue(false);
        let captured = capture(&catalogue);
        assert_eq!(
            captured.descriptor("forked").unwrap().agent(),
            Some("trusted")
        );
        for (name, field) in [("agent-without-fork", "agent"), ("temporary", "shell")] {
            let diagnostic = captured
                .diagnostics()
                .iter()
                .find(|diagnostic| diagnostic.name() == name)
                .expect("unsupported field diagnostic");
            assert!(diagnostic.message().contains(field));
        }
        assert!(
            captured
                .diagnostics()
                .iter()
                .all(|diagnostic| { diagnostic.name() != "hooked" })
        );
        assert!(
            captured
                .descriptor("hooked")
                .expect("hooked skill is admitted")
                .hooks()
                .is_empty()
        );
    }

    /// `bare` is a declaration about a child run, so it needs a child run.
    ///
    /// An inline skill body is the parent's own turn — there is no separate
    /// posture to lower — so accepting `bare` there would silently mean
    /// nothing. Refusing it says so at the file.
    #[test]
    fn bare_is_declared_only_on_a_skill_that_forks_a_child_to_hold_it() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "sealed",
            &valid_for("sealed", "context: fork\nbare: true"),
            "body",
        );
        roots.write(
            &roots.user,
            "inherits",
            &valid_for("inherits", "context: fork"),
            "body",
        );
        roots.write(
            &roots.user,
            "bare-inline",
            &valid_for("bare-inline", "bare: true"),
            "body",
        );
        let captured = capture(&roots.catalogue(false));
        assert!(
            captured
                .descriptor("sealed")
                .expect("a forked skill may declare itself bare")
                .bare()
        );
        assert!(
            !captured
                .descriptor("inherits")
                .expect("a forked skill is admitted without the field")
                .bare(),
            "a subagent inherits its parent's posture unless the role refuses it"
        );
        let diagnostic = captured
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.name() == "bare-inline")
            .expect("an inline skill declaring itself bare is refused");
        assert!(diagnostic.message().contains("bare"));
        assert!(captured.descriptor("bare-inline").is_none());
    }

    #[test]
    fn agent_must_resolve_through_the_existing_trusted_profile_owner() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "known-profile",
            &valid_for("known-profile", "context: fork\nagent: trusted"),
            "body",
        );
        roots.write(
            &roots.user,
            "missing-profile",
            &valid_for("missing-profile", "context: fork\nagent: absent"),
            "body",
        );

        let profile_layer = ConfigLayer::parse(Source::User, "[profile.trusted]\n").unwrap();
        ego_core::config::resolve(
            std::slice::from_ref(&profile_layer),
            Trust::Trusted,
            Some("trusted"),
        )
        .expect("the existing profile owner must resolve the known profile");
        // The final narrow API consumes the existing ConfigLayer/profile owner
        // data. It must not grow a second profile parser or accept a name based
        // only on identifier grammar.
        let catalogue = SkillCatalogue::discover_with_profile_layers(
            roots.discovery(false),
            &["help"],
            &[profile_layer],
        )
        .expect("profile diagnostics are non-fatal");
        let captured = capture(&catalogue);
        assert_eq!(
            captured.descriptor("known-profile").unwrap().agent(),
            Some("trusted")
        );
        let diagnostic = captured
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.name() == "missing-profile")
            .expect("unknown profile diagnostic");
        assert!(diagnostic.message().contains("existing profile"));
    }

    #[test]
    fn skill_file_and_frontmatter_bounds_fail_closed_before_body_disclosure() {
        let roots = Roots::new();
        let oversized = roots.user.join("oversized/SKILL.md");
        fs::create_dir_all(oversized.parent().unwrap()).unwrap();
        fs::write(&oversized, "x".repeat(MAX_FILE_BYTES + 1)).unwrap();
        let malformed = roots.write(
            &roots.user,
            "malformed",
            "name: malformed\nallowed-tools: &x read",
            "body",
        );
        let catalogue = roots.catalogue(false);
        let captured = capture(&catalogue);
        for (name, path) in [("oversized", oversized), ("malformed", malformed)] {
            let diagnostic = captured
                .diagnostics()
                .iter()
                .find(|diagnostic| diagnostic.name() == name)
                .expect("bounded admission diagnostic");
            assert_eq!(diagnostic.source_path(), path);
            assert!(!diagnostic.to_string().contains("body"));
        }
    }
}

#[test]
fn bundled_context_skill_is_available_without_filesystem_installation() {
    // Catches: clean installations advertise the tool without its safety guidance.
    let roots = Roots::new();
    let catalogue = roots.catalogue(false);
    let captured = capture(&catalogue);
    let spec = captured
        .descriptor("context-management")
        .expect("bundled guidance");
    assert_eq!(spec.source(), SkillSourceAuthority::System);
    assert!(
        captured
            .core()
            .authorize_model("context-management")
            .is_ok()
    );
}

#[test]
fn bundled_context_skill_does_not_hide_a_collision() {
    // Catches: a project silently replaces safety guidance with the bundled name.
    let roots = Roots::new();
    roots.write(
        &roots.project,
        "context-management",
        &valid_for("context-management", ""),
        "untrusted replacement",
    );
    let catalogue = roots.catalogue(true);
    let captured = capture(&catalogue);
    assert!(
        captured
            .diagnostics()
            .iter()
            .any(|entry| entry.name() == "context-management" && entry.is_collision())
    );
    assert!(captured.descriptor("context-management").is_none());
}

mod regression_case {
    use super::*;
    use crate::capability_roots::discover_capability_roots;

    fn discovery(roots: &Roots, workspace: &Path, trusted: bool) -> SkillDiscovery {
        SkillDiscovery {
            roots: discover_capability_roots(
                workspace,
                trusted,
                roots.system.parent().unwrap(),
                roots.user.parent().unwrap(),
                roots.user.parent().unwrap().parent(),
                "skills",
            ),
            trusted_project: trusted,
        }
    }

    #[test]
    fn ancestor_and_user_roots_are_not_missed_or_scanned_beyond_repository() {
        // Catches: nested launches miss portable roots or inherit outside-repo skills.
        let roots = Roots::new();
        let workspace = roots.project.parent().unwrap().parent().unwrap();
        fs::create_dir(workspace.join(".git")).unwrap();
        let nested = workspace.join("src/nested");
        fs::create_dir_all(&nested).unwrap();
        for (base, name) in [
            (workspace.join(".agents/skills"), "ancestor-agents"),
            (nested.join(".claude/skills"), "nested-claude"),
            (roots._dir.path().join("home/.agents/skills"), "user-agents"),
            (roots._dir.path().join("home/.claude/skills"), "user-claude"),
            (roots._dir.path().join(".agents/skills"), "outside-repo"),
        ] {
            roots.write(&base, name, &valid_for(name, ""), name);
        }
        let catalogue = SkillCatalogue::discover(discovery(&roots, &nested, true), &[]).unwrap();
        let captured = capture(&catalogue);
        for name in [
            "ancestor-agents",
            "nested-claude",
            "user-agents",
            "user-claude",
        ] {
            assert!(captured.descriptor(name).is_some(), "{name}");
        }
        assert!(captured.descriptor("outside-repo").is_none());
        let untrusted = SkillCatalogue::discover(discovery(&roots, &nested, false), &[]).unwrap();
        assert!(capture(&untrusted).descriptor("ancestor-agents").is_none());
        assert!(capture(&untrusted).descriptor("nested-claude").is_none());
        assert!(capture(&untrusted).descriptor("user-agents").is_some());
    }

    #[test]
    fn nearest_project_then_namespace_then_user_then_system_wins() {
        // Catches: directory iteration order selects the wrong instruction body.
        let roots = Roots::new();
        let workspace = roots.project.parent().unwrap().parent().unwrap();
        fs::write(workspace.join(".git"), "gitdir: elsewhere").unwrap();
        let nested = workspace.join("nested");
        fs::create_dir_all(&nested).unwrap();
        for root in [
            &roots.system,
            &roots.user,
            &roots.project,
            &workspace.join(".agents/skills"),
            &nested.join(".agents/skills"),
            &nested.join(".ego/skills"),
        ] {
            roots.write(root, "winner", &valid_for("winner", ""), "body");
        }
        let catalogue = SkillCatalogue::discover(discovery(&roots, &nested, true), &[]).unwrap();
        let captured = capture(&catalogue);
        assert!(
            captured.render_list().contains(
                &nested
                    .join(".ego/skills/winner/SKILL.md")
                    .display()
                    .to_string()
            )
        );
        assert_eq!(
            captured.descriptor("winner").unwrap().source(),
            SkillSourceAuthority::Project
        );
        assert!(captured.diagnostics().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn boss_layout_symlink_is_one_invocable_skill() {
        // Catches: portable aliases cause collisions and remove the usable skill.
        let roots = Roots::new();
        let workspace = roots.project.parent().unwrap().parent().unwrap();
        let agents = workspace.join(".agents/skills");
        roots.write(&agents, "foo", &valid_for("foo", ""), "Portable body.");
        let claude = workspace.join(".claude/skills");
        fs::create_dir_all(&claude).unwrap();
        std::os::unix::fs::symlink("../../.agents/skills/foo", claude.join("foo")).unwrap();
        let catalogue = SkillCatalogue::discover(discovery(&roots, workspace, true), &[]).unwrap();
        let captured = capture(&catalogue);
        assert_eq!(
            captured
                .descriptors()
                .iter()
                .filter(|spec| spec.name() == "foo")
                .count(),
            1
        );
        assert!(captured.core().authorize_user("foo").is_ok());
        assert!(captured.diagnostics().is_empty());
        assert!(
            captured
                .render_list()
                .contains(&agents.join("foo/SKILL.md").display().to_string())
        );
    }
}
