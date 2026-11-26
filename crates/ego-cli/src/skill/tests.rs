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
        let system = dir.path().join("etc").join("ego").join("skills");
        let user = dir.path().join("home").join(".ego").join("skills");
        let project = dir.path().join("workspace").join(".ego").join("skills");
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

// Catches: the default /skills listing leaves the user without commands to inspect or run skills.
#[test]
fn skill_summary_names_available_controls() {
    let roots = Roots::new();
    let text = capture(&roots.catalogue(false))
        .render_summary(crate::render::Capabilities::plain(), false);
    for command in [
        "/skills details",
        "/skills diagnose",
        "/skills rescan",
        "/skills invoke NAME",
    ] {
        assert!(text.contains(command), "missing {command}: {text}");
    }
}

// Catches missing provenance and a badge style leaking into plain output.
#[test]
fn skill_summary_shows_winning_origins_with_or_without_color() {
    use crate::render::{Capabilities, Depth};
    let roots = Roots::new();
    for (root, name) in [
        (&roots.project, "local"),
        (&roots.user, "personal"),
        (&roots.system, "managed"),
    ] {
        roots.write(root, name, &valid_for(name, ""), "body");
    }
    let captured = capture(&roots.catalogue(true));
    let caps = Capabilities::plain();
    let plain = captured.render_summary(caps, false);
    for badge in ["[project] .ego", "[user]", "[system]", "[built-in]"] {
        assert!(plain.contains(badge), "{plain}");
    }
    assert!(!plain.contains('\x1b'));
    for (light, rgb) in [(false, "156;175;198"), (true, "75;96;120")] {
        let colored = captured.render_summary(
            Capabilities {
                depth: Depth::TrueColor,
                ..caps
            },
            light,
        );
        assert!(
            colored.contains(&format!("\x1b[38;2;{rgb}m[project]")),
            "{colored:?}"
        );
        assert!(
            !colored.contains("m.ego"),
            "namespace should not share badge styling"
        );
    }
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

mod skills_075 {
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
    fn canonical_allowed_tools_scalar_is_admitted_and_invalid_selectors_refuse_by_name() {
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
            (
                "unknown-form",
                "allowed-tools: \"read definitely-not-a-tool\"",
            ),
            ("empty-form", "allowed-tools: \"\""),
        ] {
            roots.write(&roots.user, name, &valid_for(name, field), "body");
        }
        let candidate = SkillCatalogue::discover(roots.discovery(false), &["read", "grep"])
            .expect("candidate diagnostics are non-fatal");
        let captured = capture(&candidate);
        for name in ["unknown-form", "empty-form"] {
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
            "name: stable\nallowed_tools: read",
            "bad body",
        );
        roots.write(&roots.user, "new-invalid", "name: INVALID", "bad body");

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
                    && diagnostic.message().contains("allowed_tools"))
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
        assert!(captured.descriptor("temporary").is_some());
        let diagnostic = captured
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.name() == "agent-without-fork")
            .expect("agent without fork diagnostic");
        assert!(diagnostic.message().contains("agent"));
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

mod skill_roots_474 {
    use super::*;
    use crate::capability_roots::discover_capability_roots;

    fn discovery(roots: &Roots, workspace: &Path, trusted: bool) -> SkillDiscovery {
        SkillDiscovery {
            command_roots: Vec::new(),
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
    fn repository_skill_alias_reports_trust_root_status_and_precedence() {
        // Catches: missing project discovery, misleading diagnostics, or shadowing that blocks a rescan.
        let roots = Roots::new();
        let workspace = roots.project.parent().unwrap().parent().unwrap();
        let alias = workspace.join(".github").join("skills");
        let portable = workspace.join(".agents").join("skills");
        let winner = roots.write(&portable, "shared", &valid_for("shared", ""), "winner");
        roots.write(&alias, "shared", &valid_for("shared", ""), "shadowed");
        roots.write(
            &alias,
            "repository-skill",
            &valid_for("repository-skill", ""),
            "admitted",
        );

        let untrusted = SkillCatalogue::discover(discovery(&roots, workspace, false), &[]).unwrap();
        let capture_untrusted = capture(&untrusted);
        assert!(capture_untrusted.descriptor("repository-skill").is_none());
        assert!(
            capture_untrusted
                .render_diagnostics()
                .contains("skipped: workspace is not trusted")
        );

        let trusted = SkillCatalogue::discover(discovery(&roots, workspace, true), &[]).unwrap();
        let captured = capture(&trusted);
        assert!(captured.descriptor("repository-skill").is_some());
        let report = captured.render_diagnostics();
        assert!(
            report.contains(&format!("shadowed by {}", winner.display())),
            "{report}"
        );
        assert!(
            report.contains(&format!(
                "{}: not found",
                workspace.join(".codex").join("skills").display()
            )),
            "{report}"
        );
        assert!(
            report.contains(&format!("{}: no SKILL.md files", roots.project.display())),
            "{report}"
        );
        assert!(trusted.prepare_rescan().unwrap().published());
    }

    #[cfg(unix)]
    #[test]
    fn repository_skill_root_link_cannot_import_an_external_directory() {
        // Catches: canonicalizing the root makes an external directory look project-owned.
        let roots = Roots::new();
        let workspace = roots.project.parent().unwrap().parent().unwrap();
        let external = roots._dir.path().join("external");
        roots.write(
            &external,
            "outside",
            &valid_for("outside", ""),
            "not project content",
        );
        fs::create_dir(workspace.join(".github")).unwrap();
        std::os::unix::fs::symlink(external, workspace.join(".github/skills")).unwrap();
        let trusted = SkillCatalogue::discover(discovery(&roots, workspace, true), &[]).unwrap();
        let captured = capture(&trusted);
        assert!(captured.descriptor("outside").is_none());
        assert!(
            captured
                .render_diagnostics()
                .contains("outside its project directory")
        );
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
                    .join(".ego")
                    .join("skills")
                    .join("winner")
                    .join("SKILL.md")
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

mod managed_roots_511 {
    use super::*;
    use crate::capability_roots::{discover_capability_roots, register_external_skill_roots};

    #[test]
    fn managed_and_codex_cache_files_reach_catalogue_without_admitting_plugin_caches() {
        // Catches: registered foreign roots never reach the real catalogue, or
        // installing a disabled plugin implicitly grants its skills admission.
        let fixture = Roots::new();
        let base = fixture._dir.path();
        let workspace = base.join("workspace");
        let home = base.join("home");
        let codex = home.join("custom-codex");
        let claude = home.join("custom-claude");
        let managed = base.join("managed-claude");
        let admin = base.join("managed-codex");
        for (root, name) in [
            (codex.join("skills"), "codex-user"),
            (codex.join("skills/.system"), "codex-bundled"),
            (claude.join("skills"), "claude-custom"),
            (managed.join(".claude/skills"), "claude-admin"),
            (admin.join("skills"), "codex-admin"),
            (workspace.join(".codex/skills"), "codex-project"),
            (
                claude.join("plugins/cache/demo/1/skills"),
                "disabled-plugin",
            ),
        ] {
            fixture.write(&root, name, &valid_for(name, ""), "real skill body");
        }
        for trusted in [false, true] {
            let mut roots = discover_capability_roots(
                &workspace,
                trusted,
                fixture.system.parent().unwrap(),
                fixture.user.parent().unwrap(),
                Some(&home),
                "skills",
            );
            register_external_skill_roots(
                &mut roots,
                Some(&codex),
                Some(&claude),
                &managed,
                &admin,
            );
            let catalogue = SkillCatalogue::discover(
                SkillDiscovery {
                    roots,
                    command_roots: Vec::new(),
                    trusted_project: trusted,
                },
                &[],
            )
            .unwrap();
            let admitted = capture(&catalogue);
            for (name, authority) in [
                ("codex-user", SkillSourceAuthority::User),
                ("codex-bundled", SkillSourceAuthority::User),
                ("claude-custom", SkillSourceAuthority::User),
                ("claude-admin", SkillSourceAuthority::System),
                ("codex-admin", SkillSourceAuthority::System),
            ] {
                assert_eq!(admitted.descriptor(name).unwrap().source(), authority);
            }
            assert_eq!(admitted.descriptor("codex-project").is_some(), trusted);
            assert!(admitted.descriptor("disabled-plugin").is_none());
            assert!(admitted.diagnostics().is_empty());
        }
    }
}

#[cfg(unix)]
mod symlink_roots_521 {
    use super::*;
    use crate::capability_roots::CapabilityRoot;
    use std::os::unix::fs::symlink;

    #[test]
    fn user_owned_external_symlink_loads_once_and_is_invocable() {
        // Catches: user skill symlinks outside their roots are refused or duplicated.
        let roots = Roots::new();
        let external = roots._dir.path().join("external");
        let source = roots.write(
            &external,
            "linked",
            &valid_for("linked", ""),
            "Linked body.",
        );
        let home = roots._dir.path().join("home");
        let mut discovery = roots.discovery(false);
        for namespace in [".agents", ".claude", ".codex"] {
            let root = home.join(namespace).join("skills");
            fs::create_dir_all(&root).unwrap();
            symlink(source.parent().unwrap(), root.join("linked")).unwrap();
            discovery.roots.insert(
                0,
                CapabilityRoot {
                    source: SkillSourceAuthority::User,
                    path: root.clone(),
                },
            );
            let catalogue = SkillCatalogue::discover(discovery.clone(), &[]).unwrap();
            let captured = capture(&catalogue);
            assert!(
                captured.diagnostics().is_empty(),
                "{:?}",
                captured.diagnostics()
            );
            assert_eq!(
                captured
                    .descriptors()
                    .iter()
                    .filter(|spec| spec.name() == "linked")
                    .count(),
                1
            );
            let skill = captured
                .descriptor("linked")
                .expect("user symlink admitted");
            assert_eq!(skill.source(), SkillSourceAuthority::User);
            // Catches: discovery drops the external symlink target before invocation.
            let resource_root = skill
                .resource_root()
                .expect("discovery-bound resource root");
            assert_eq!(
                resource_root.as_path(),
                fs::canonicalize(source.parent().unwrap()).unwrap()
            );
            fs::write(source.parent().unwrap().join("README.md"), "external guide").unwrap();
            assert_eq!(
                fs::read_to_string(resource_root.as_path().join("README.md")).unwrap(),
                "external guide"
            );
            assert_eq!(
                skill
                    .render(&ego_core::skill::SkillOperation::Invoke {
                        name: "linked".to_owned(),
                        arguments: String::new(),
                        bare: false,
                    })
                    .unwrap(),
                "Linked body.\n"
            );
            assert!(captured.core().authorize_user("linked").is_ok());
            assert!(
                captured
                    .render_list()
                    .contains(&root.join("linked/SKILL.md").display().to_string())
            );
        }
    }

    #[test]
    fn project_and_system_external_symlinks_remain_refused() {
        // Catches: relaxing user symlinks also admits an external project or system skill.
        let roots = Roots::new();
        let external = roots._dir.path().join("external");
        let source = roots.write(
            &external,
            "linked",
            &valid_for("linked", ""),
            "Outside body.",
        );
        for (authority, root) in [
            (SkillSourceAuthority::Project, &roots.project),
            (SkillSourceAuthority::System, &roots.system),
        ] {
            symlink(source.parent().unwrap(), root.join("linked")).unwrap();
            let discovery = SkillDiscovery {
                roots: vec![CapabilityRoot {
                    source: authority,
                    path: root.clone(),
                }],
                command_roots: Vec::new(),
                trusted_project: true,
            };
            let catalogue = SkillCatalogue::discover(discovery, &[]).unwrap();
            let captured = capture(&catalogue);
            assert!(captured.descriptor("linked").is_none());
            assert!(captured.core().authorize_user("linked").is_err());
            assert_eq!(captured.diagnostics().len(), 1);
            let diagnostic = &captured.diagnostics()[0];
            assert_eq!(diagnostic.source(), authority);
            assert_eq!(
                diagnostic.message(),
                "resolves outside its canonical skill root"
            );
        }
    }
}

mod reference_frontmatter_996 {
    use super::*;
    use ego_core::skill::SkillOperation;

    fn invoke(arguments: &str) -> SkillOperation {
        SkillOperation::Invoke {
            name: "sample".to_owned(),
            arguments: arguments.to_owned(),
            bare: false,
        }
    }

    fn admitted(header: &str) -> ego_core::skill::SkillSpec {
        let roots = Roots::new();
        roots.write(&roots.user, "sample", header, "Describe the task.");
        let captured = capture(&roots.catalogue(false));
        captured
            .descriptor("sample")
            .unwrap_or_else(|| panic!("{}", captured.render_diagnostics()))
            .clone()
    }

    // Catches: an inert extension prevents admission of an otherwise valid skill.
    #[test]
    fn unknown_fields_do_not_hide_skill() {
        admitted(&valid_for("sample", "extension:\n  nested: [value]"));
    }

    // Catches: a misspelled authority ceiling is silently ignored as an extension.
    #[test]
    fn allowed_tools_underscore_stays_refused() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "sample",
            &valid_for("sample", "allowed_tools: read"),
            "body",
        );
        let captured = capture(&roots.catalogue(false));
        assert!(captured.descriptor("sample").is_none());
        assert!(
            captured
                .render_diagnostics()
                .contains("use exact field `allowed-tools`")
        );
    }

    // Catches: a misspelled subtractive rule silently loses the user's denial.
    #[test]
    fn disallowed_tools_underscore_stays_refused() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "sample",
            &valid_for("sample", "disallowed_tools: read"),
            "body",
        );
        let captured = capture(&roots.catalogue(false));
        assert!(captured.descriptor("sample").is_none());
        assert!(
            captured
                .render_diagnostics()
                .contains("use exact field `disallowed-tools`")
        );
    }

    // Catches: a missing name prevents use of the directory's command name.
    #[test]
    fn missing_name_uses_directory() {
        assert_eq!(admitted("description: Task summary").name(), "sample");
    }

    // Catches: a missing description hides the skill instead of deriving body guidance.
    #[test]
    fn missing_description_uses_body() {
        for header in ["name: sample", ""] {
            assert_eq!(
                admitted(header).descriptor().description,
                "Describe the task."
            );
        }
    }

    // Catches: an explicit command alias is rejected when the directory differs.
    #[test]
    fn explicit_name_may_differ_from_directory() {
        let roots = Roots::new();
        roots.write(&roots.user, "directory", &valid_for("alias", ""), "body");
        let captured = capture(&roots.catalogue(false));
        assert!(captured.descriptor("alias").is_some());
        assert!(captured.descriptor("directory").is_none());
    }

    // Catches: YAML boolean aliases refuse or invert the advertised invocation control.
    #[test]
    fn boolean_aliases_control_visibility() {
        for (token, expected) in [
            ("yes", true),
            ("no", false),
            ("on", true),
            ("off", false),
            ("1", true),
            ("0", false),
        ] {
            let spec = admitted(&valid_for("sample", &format!("user-invocable: {token}")));
            assert_eq!(spec.descriptor().user_invocable, expected, "{token}");
        }
    }

    // Catches: string argument declarations never bind named source placeholders.
    #[test]
    fn string_arguments_bind_named_positions() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "sample",
            &valid_for("sample", "arguments: first second"),
            "$second/$first",
        );
        let captured = capture(&roots.catalogue(false));
        let rendered = captured
            .descriptor("sample")
            .unwrap()
            .render(&invoke("one two"))
            .unwrap();
        assert_eq!(rendered.trim(), "two/one");
    }

    // Catches: a list tool ceiling refuses instead of narrowing to the listed tools.
    #[test]
    fn allowed_tools_list_loads() {
        let spec = admitted(&valid_for("sample", "allowed-tools: [read, grep]"));
        assert_eq!(
            spec.allowed_tools()
                .unwrap()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["grep", "read"]
        );
    }

    // Catches: comma-separated tool ceilings refuse despite naming canonical tools.
    #[test]
    fn allowed_tools_comma_scalar_loads() {
        let spec = admitted(&valid_for("sample", "allowed-tools: read, grep"));
        assert_eq!(
            spec.allowed_tools()
                .unwrap()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["grep", "read"]
        );
    }

    // Catches: list-form denials fail to map reference names to actual effect tools.
    #[test]
    fn disallowed_tools_list_maps_reference_names() {
        let spec = admitted(&valid_for(
            "sample",
            "disallowed-tools: [Bash, Read, Edit, Write, Glob, Grep, AskUserQuestion, WebFetch, NotebookEdit, Task]",
        ));
        assert_eq!(
            spec.disallowed_tools()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "ask_question",
                "edit",
                "exec",
                "glob",
                "grep",
                "notebook_edit",
                "read",
                "task",
                "web_fetch",
                "write"
            ]
        );
    }

    // Catches: unknown names or partial pattern words become effect-tool denials.
    #[test]
    fn disallowed_tools_scalar_ignores_unknowns_and_patterns() {
        let spec = admitted(&valid_for(
            "sample",
            "disallowed-tools: Read, Bash(echo Write *), UnknownTool, Grep",
        ));
        assert_eq!(
            spec.disallowed_tools()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["grep", "read"]
        );
    }

    // Catches: inherit is treated as a concrete model override and fails selection.
    #[test]
    fn inherit_model_does_not_override() {
        assert_eq!(
            admitted(&valid_for("sample", "model: inherit")).model(),
            None
        );
    }

    // Catches: inert shell metadata refuses or changes the skill's execution mode.
    #[test]
    fn shell_metadata_loads_without_fork() {
        for shell in ["bash", "powershell"] {
            assert_eq!(
                admitted(&valid_for("sample", &format!("shell: {shell}"))).fork(),
                None
            );
        }
    }

    // Catches: scalar or sequence metadata refuses an otherwise executable skill.
    #[test]
    fn non_map_metadata_is_inert() {
        for metadata in ["text", "[one, two]", "null", "42"] {
            assert!(
                admitted(&valid_for("sample", &format!("metadata: {metadata}")))
                    .metadata()
                    .is_empty()
            );
        }
    }

    // Catches: a long description refuses the whole skill instead of being cut for the listing.
    #[test]
    fn long_description_is_cut_not_refused() {
        let spec = admitted(&format!("name: sample\ndescription: {}", "d".repeat(2000)));
        assert_eq!(spec.descriptor().description.chars().count(), 1536);
    }

    // Catches: the unquoted `[issue-number]` hint (a YAML list) refuses the skill.
    #[test]
    fn listed_argument_hint_reads_as_bracketed_text() {
        let spec = admitted(&valid_for("sample", "argument-hint: [issue-number]"));
        assert_eq!(
            spec.descriptor().argument_hint.as_deref(),
            Some("[issue-number]")
        );
    }

    // Catches: free-form metadata with a number or nested value, or a non-string
    // license, refuses a skill that only carries data for outside tools.
    #[test]
    fn metadata_and_license_never_refuse_a_skill() {
        let spec = admitted(&valid_for(
            "sample",
            "license: 2\nmetadata:\n  version: 1\n  owner: team\n  nested:\n    a: b",
        ));
        assert_eq!(
            spec.metadata().get("version").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            spec.metadata().get("owner").map(String::as_str),
            Some("team")
        );
        assert!(!spec.metadata().contains_key("nested"));
    }

    // Catches: `effort: High` or `model: Inherit` is refused or pins a model named "Inherit".
    #[test]
    fn effort_and_inherit_ignore_letter_case() {
        let spec = admitted(&valid_for("sample", "effort: High\nmodel: Inherit"));
        assert_eq!(spec.effort(), Some("high"));
        assert_eq!(spec.model(), None);
    }

    // Catches: a reference tool name in `allowed-tools` is refused without saying
    // that ego narrows, so the author cannot tell why a valid-looking skill fails.
    #[test]
    fn foreign_allowed_tool_name_explains_that_ego_narrows() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "sample",
            &valid_for("sample", "allowed-tools: Read Grep"),
            "Describe the task.",
        );
        let captured = capture(&roots.catalogue(false));
        let diagnostic = captured
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.name() == "sample")
            .expect("foreign name is refused");
        assert!(diagnostic.message().contains("narrows"));
    }

    // Catches: an unquoted colon in a description refuses a skill that the reference
    // format loads, or the loaded skill keeps the broken description text.
    #[test]
    fn text_that_is_not_yaml_loads_without_metadata() {
        let spec = admitted("description: Use when: the build fails");
        assert_eq!(spec.name(), "sample");
        assert_eq!(spec.descriptor().description, "Describe the task.");
    }

    // Catches: dropping an unparsable header widens the skill: a denial, a
    // visibility flag or a tool ceiling written next to the broken line vanishes.
    #[test]
    fn unparsable_header_naming_a_restricting_field_is_refused_with_a_hint() {
        for field in [
            "disallowed-tools: write",
            "disable-model-invocation: true",
            "allowed-tools: read",
            "context: fork",
        ] {
            let roots = Roots::new();
            roots.write(
                &roots.user,
                "sample",
                &format!("description: Use when: x\n{field}"),
                "Describe the task.",
            );
            let captured = capture(&roots.catalogue(false));
            assert!(captured.descriptor("sample").is_none(), "{field}");
            let message = captured.diagnostics()[0].message().to_owned();
            assert!(message.contains("Quote a value"), "{message}");
            assert!(
                message.contains(field.split(':').next().unwrap()),
                "{message}"
            );
        }
    }

    // Catches: a reference skill's `paths` activation globs are read as ego's resource
    // policy, so the skill loads but cannot read its own files; or the globs never
    // reach the model and the hint is lost.
    #[test]
    fn paths_are_an_activation_hint_and_resource_paths_are_the_policy() {
        let spec = admitted(&valid_for(
            "sample",
            "when_to_use: Use for TypeScript.\npaths: \"src/**/*.ts, docs/*.md\"\nresource-paths:\n  - refs/**",
        ));
        assert_eq!(
            spec.descriptor().trigger.as_deref(),
            Some(
                "Use for TypeScript. Applies when working with files matching: src/**/*.ts, docs/*.md"
            )
        );
        let listed = admitted(&valid_for("sample", "paths:\n  - a/**\n  - b/**"));
        assert_eq!(
            listed.descriptor().trigger.as_deref(),
            Some("Applies when working with files matching: a/**, b/**")
        );
    }

    // Catches: `agent: general-purpose` refuses a skill that only asks for the ordinary
    // child, or `agent: Explore` is refused without saying how to map it.
    #[test]
    fn reference_agent_names_map_to_no_profile_or_to_a_defined_one() {
        let roots = Roots::new();
        roots.write(
            &roots.user,
            "ordinary",
            &valid_for("ordinary", "context: fork\nagent: general-purpose"),
            "body",
        );
        roots.write(
            &roots.user,
            "explorer",
            &valid_for("explorer", "context: fork\nagent: Explore"),
            "body",
        );
        let layer = ConfigLayer::parse(Source::User, "[profile.trusted]\n").unwrap();
        let catalogue = SkillCatalogue::discover_with_profile_layers(
            roots.discovery(false),
            &["help"],
            &[layer],
        )
        .expect("profile diagnostics are non-fatal");
        let captured = capture(&catalogue);
        assert_eq!(captured.descriptor("ordinary").unwrap().agent(), None);
        assert!(captured.descriptor("explorer").is_none());
        let message = captured
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.name() == "explorer")
            .expect("an unmapped reference agent is refused")
            .message()
            .to_owned();
        assert!(message.contains("[profile.Explore]"), "{message}");
        let defined = ConfigLayer::parse(Source::User, "[profile.Explore]\n").unwrap();
        let catalogue = SkillCatalogue::discover_with_profile_layers(
            roots.discovery(false),
            &["help"],
            &[defined],
        )
        .unwrap();
        assert_eq!(
            capture(&catalogue).descriptor("explorer").unwrap().agent(),
            Some("Explore")
        );
    }

    // Catches: the owner's `[skills]` list is ignored, hides a skill from the user too
    // when only the model should lose it, leaves a model-only skill with no audience,
    // or touches skills it does not name.
    #[test]
    fn owner_config_hides_skills_by_name() {
        let roots = Roots::new();
        for name in ["shown", "hidden", "gone", "model-only"] {
            let extra = if name == "model-only" {
                "user-invocable: false"
            } else {
                ""
            };
            roots.write(&roots.user, name, &valid_for(name, extra), "body");
        }
        let layer = ConfigLayer::parse(
            Source::User,
            "[skills]\nhide = [\"hidden\", \"model-only\"]\noff = [\"gone\"]\n",
        )
        .unwrap();
        let catalogue = SkillCatalogue::discover_with_profile_layers(
            roots.discovery(false),
            &["help"],
            &[layer],
        )
        .unwrap();
        let captured = capture(&catalogue);
        let flags = |name: &str| {
            captured.descriptor(name).map(|spec| {
                (
                    spec.descriptor().user_invocable,
                    spec.descriptor().model_invocable,
                )
            })
        };
        assert_eq!(flags("shown"), Some((true, true)));
        assert_eq!(flags("hidden"), Some((true, false)));
        assert_eq!(flags("gone"), None);
        assert_eq!(flags("model-only"), None);
        // Hiding leaves no diagnostic: a diagnostic would stop a rescan from publishing.
        assert!(captured.diagnostics().is_empty());
    }

    // Catches: source-directory expansion rescans caller input or inserted path bytes.
    #[test]
    fn source_directory_expands_only_source_tokens() {
        let roots = Roots::new();
        let path = roots.write(
            &roots.user,
            "${CLAUDE_SKILL_DIR}-$ARGUMENTS",
            &valid_for("sample", ""),
            "${CLAUDE_SKILL_DIR}/asset $ARGUMENTS",
        );
        let captured = capture(&roots.catalogue(false));
        let spec = captured.descriptor("sample").unwrap();
        let rendered = spec.render(&invoke("${CLAUDE_SKILL_DIR}")).unwrap();
        assert_eq!(
            rendered.trim(),
            format!(
                "{}/asset ${{CLAUDE_SKILL_DIR}}",
                path.canonicalize().unwrap().parent().unwrap().display()
            )
        );
    }
}
