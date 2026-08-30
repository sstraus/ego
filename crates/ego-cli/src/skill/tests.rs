//! RED contract for M4 S3: shared YAML extraction and skill admission.
//!
//! The service used here is deliberately the CLI-owned filesystem adapter. It
//! must reuse the bounded envelope used by agent files while leaving policy,
//! source authority, and immutable catalogue facts in their existing owners.

use std::fs;
use std::path::{Path, PathBuf};

use super::{SkillCatalogue, SkillDiscovery, SkillSourceAuthority};
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

fn valid_for(name: &str, frontmatter: &str) -> String {
    format!("name: {name}\ndescription: Explain a failing check.\n{frontmatter}")
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
        let descriptor = catalogue
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
        let mut sources = catalogue
            .descriptors()
            .iter()
            .map(|descriptor| (descriptor.name(), descriptor.source()))
            .collect::<Vec<_>>();
        sources.sort_by_key(|(name, _)| *name);
        assert_eq!(
            sources,
            vec![
                ("project-skill", SkillSourceAuthority::Project),
                ("system-skill", SkillSourceAuthority::System),
                ("user-skill", SkillSourceAuthority::User),
            ]
        );
        assert!(catalogue.diagnostics().iter().all(|diagnostic| !diagnostic
            .source_path()
            .to_string_lossy()
            .contains(".claude")));
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
        assert!(catalogue
            .descriptors()
            .iter()
            .any(|descriptor| descriptor.name() == "canonical"));

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
        for name in [
            "list-form",
            "comma-form",
            "unknown-form",
            "empty-form",
            "deprecated-form",
        ] {
            let diagnostic = candidate
                .diagnostics()
                .iter()
                .find(|diagnostic| diagnostic.name() == name)
                .expect("exact rejected skill diagnostic");
            assert!(diagnostic.message().contains("allowed-tools"));
        }
    }

    #[test]
    fn duplicate_names_and_fixed_action_collisions_are_unavailable_without_shadowing() {
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
        assert!(catalogue
            .descriptors()
            .iter()
            .all(|descriptor| descriptor.name() != "duplicate" && descriptor.name() != "skills"));
        for name in ["duplicate", "skills"] {
            assert!(catalogue
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.name() == name && diagnostic.is_collision()));
        }
    }

    #[test]
    fn untrusted_project_skills_are_not_read_or_admitted() {
        let roots = Roots::new();
        let project = roots.write(
            &roots.project,
            "project-only",
            &valid_for("project-only", ""),
            "secret project body",
        );
        let catalogue = roots.catalogue(false);
        assert!(catalogue
            .descriptors()
            .iter()
            .all(|descriptor| descriptor.name() != "project-only"));
        assert!(catalogue
            .diagnostics()
            .iter()
            .any(
                |diagnostic| diagnostic.source() == SkillSourceAuthority::Project
                    && diagnostic.source_path() == project
            ));
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
        let mut catalogue = roots.catalogue(false);
        let before = catalogue.snapshot_digest();
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

        catalogue
            .reload(roots.discovery(false), &["help"])
            .expect("reload returns last good");
        assert_eq!(catalogue.snapshot_digest(), before);
        assert!(catalogue
            .descriptors()
            .iter()
            .any(|descriptor| descriptor.name() == "stable"));
        assert!(catalogue
            .descriptors()
            .iter()
            .all(|descriptor| descriptor.name() != "new-invalid"));
        assert!(catalogue
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.name() == "stable"
                && diagnostic.message().contains("unknown")));
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
        assert_eq!(
            catalogue.descriptor("forked").unwrap().agent(),
            Some("trusted")
        );
        for (name, field) in [
            ("agent-without-fork", "agent"),
            ("temporary", "shell"),
            ("hooked", "hooks"),
        ] {
            let diagnostic = catalogue
                .diagnostics()
                .iter()
                .find(|diagnostic| diagnostic.name() == name)
                .expect("unsupported field diagnostic");
            assert!(diagnostic.message().contains(field));
        }
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
        for (name, path) in [("oversized", oversized), ("malformed", malformed)] {
            let diagnostic = catalogue
                .diagnostics()
                .iter()
                .find(|diagnostic| diagnostic.name() == name)
                .expect("bounded admission diagnostic");
            assert_eq!(diagnostic.source_path(), path);
            assert!(!diagnostic.to_string().contains("body"));
        }
    }
}
