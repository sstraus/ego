//! Markdown command discovery uses the same root policy and publication as skills.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use ego_core::event::skill::{
    SkillContentDigest, SkillDescriptorEvidence, SkillId, SkillSourceAuthority, SkillSourceLocation,
};
use ego_core::skill::SkillSpec;

type Discovery = (
    Vec<(SkillSpec, PathBuf)>,
    Vec<(String, SkillSourceAuthority, PathBuf, String)>,
);

pub(crate) fn discover_roots(roots: &[crate::capability_roots::CapabilityRoot]) -> Discovery {
    let mut specs = Vec::new();
    let mut errors = Vec::new();
    let mut names = BTreeSet::new();
    for root in roots {
        let mut paths = Vec::new();
        walk(&root.path, &root.path, 0, &mut paths);
        paths.sort();
        for path in paths {
            let Some(relative) = path.strip_prefix(&root.path).ok() else {
                continue;
            };
            let Some(stem) = relative
                .with_extension("")
                .to_str()
                .map(|s| s.replace(['/', '\\'], ":"))
            else {
                continue;
            };
            if !names.insert(stem.clone()) {
                continue;
            }
            match load(root.source, &path, &stem) {
                Ok(spec) => specs.push((spec, path)),
                Err(error) => errors.push((stem, root.source, path, error)),
            }
        }
    }
    (specs, errors)
}

pub(crate) fn project_files(workspace: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in crate::capability_roots::discover_capability_roots(
        workspace,
        true,
        Path::new("/etc/ego"),
        Path::new(""),
        None,
        "commands",
    ) {
        if root.source == SkillSourceAuthority::Project {
            walk(&root.path, &root.path, 0, &mut files);
        }
    }
    files
}

fn walk(root: &Path, path: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > 16 || files.len() >= 256 {
        return;
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    if meta.is_file() {
        if path.extension().is_some_and(|ext| ext == "md") && path.starts_with(root) {
            files.push(path.to_owned());
        }
    } else if meta.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            walk(root, &path, depth + 1, files);
        }
    }
}

fn load(source: SkillSourceAuthority, path: &Path, name: &str) -> Result<SkillSpec, String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_:".contains(ch))
    {
        return Err("invalid command name".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(crate::yaml_envelope::MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > crate::yaml_envelope::MAX_FILE_BYTES {
        return Err("command exceeds 1 MiB".into());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "command is not UTF-8")?;
    let (mut fields, body) = if text.starts_with("---\n") || text.starts_with("---\r\n") {
        let (header, body) = crate::yaml_envelope::split(text, false)?;
        (crate::yaml_envelope::mapping(header)?, body)
    } else {
        (BTreeMap::new(), text)
    };
    let description = fields
        .remove("description")
        .map(|node| crate::yaml_envelope::scalar(node).ok_or("description must be a string"))
        .transpose()?
        .unwrap_or_else(|| format!("Run {name}"));
    let argument_hint = fields
        .remove("argument-hint")
        .map(|node| crate::yaml_envelope::scalar(node).ok_or("argument-hint must be a string"))
        .transpose()?;
    if let Some(field) = fields.keys().next() {
        return Err(format!(
            "command field `{field}` is not supported; refusing rather than ignoring its behavior"
        ));
    }
    let location = SkillSourceLocation::parse(&format!(
        "commands/{}/{}.md",
        ego_core::digest::sha256_hex(path.to_string_lossy().as_bytes()),
        name
    ))
    .map_err(|e| e.to_string())?;
    let digest = SkillContentDigest::parse(&ego_core::digest::sha256_hex(&bytes))
        .map_err(|e| e.to_string())?;
    let descriptor = SkillDescriptorEvidence {
        id: SkillId::derive(source, &location, name, &digest),
        name: name.into(),
        description,
        trigger: None,
        argument_hint,
        source,
        location,
        source_digest: digest,
        user_invocable: true,
        model_invocable: false,
    };
    SkillSpec::new(descriptor, body.as_bytes().to_vec())
        .map(|spec| spec.with_metadata(BTreeMap::from([("ego-command".into(), "true".into())])))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_roots_namespace_nearest_precedence_and_untrusted_gate() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let user = dir.path().join("user");
        let system = dir.path().join("system");
        for (root, text) in [
            (workspace.join(".claude/commands/git"), "project"),
            (workspace.join(".opencode/commands/git"), "opencode"),
            (user.join("commands/git"), "user"),
        ] {
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("review.md"), text).unwrap();
        }
        let (specs, errors) = discover_roots(&crate::capability_roots::discover_capability_roots(
            &workspace, true, &system, &user, None, "commands",
        ));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].0.name(), "git:review");
        assert_eq!(specs[0].0.command_template(), Some("project"));
        let (specs, _) = discover_roots(&crate::capability_roots::discover_capability_roots(
            &workspace, false, &system, &user, None, "commands",
        ));
        assert_eq!(specs[0].0.command_template(), Some("user"));
    }

    #[test]
    fn custom_command_catalogue_drives_help_completion_and_typed_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let commands = workspace.join(".claude/commands");
        fs::create_dir_all(&commands).unwrap();
        fs::write(commands.join("review.md"), "Review $1").unwrap();
        let discovery = crate::skill::SkillDiscovery::for_workspace(&workspace, true).unwrap();
        let actions = crate::repl::actions::ActionRegistry::final_surface();
        let names = actions
            .names()
            .into_iter()
            .chain(actions.aliases())
            .collect::<Vec<_>>();
        let catalogue = crate::skill::SkillCatalogue::discover(discovery, &names).unwrap();
        actions.bind_skill_view(catalogue.view()).unwrap();
        assert!(
            actions
                .skill_menu("rev")
                .iter()
                .any(|(name, _)| name == "review")
        );
        assert!(crate::repl::render_action_help(&actions, 100).contains("/review"));
        assert!(matches!(
            actions.classify(
                actions.submit("/review target", crate::repl::actions::InputOrigin::Typed)
            ),
            crate::repl::actions::Dispatch::Skill(_)
        ));
    }

    #[test]
    fn unsupported_model_and_tool_authority_are_rejected_not_ignored() {
        let root = tempfile::tempdir().unwrap();
        for field in ["model: vendor/model", "allowed-tools: Bash", "agent: build"] {
            let path = root.path().join("review.md");
            fs::write(&path, format!("---\n{field}\n---\nReview")).unwrap();
            assert!(
                load(SkillSourceAuthority::User, &path, "review")
                    .unwrap_err()
                    .contains("not supported")
            );
        }
    }

    #[test]
    fn markdown_body_and_catalogue_metadata_are_preserved() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("review.md");
        fs::write(
            &path,
            "---\ndescription: Review changes\nargument-hint: '[file]'\n---\nReview $1 !`git diff`",
        )
        .unwrap();
        let spec = load(SkillSourceAuthority::User, &path, "git:review").unwrap();
        assert_eq!(spec.command_template(), Some("Review $1 !`git diff`"));
        assert_eq!(spec.descriptor().argument_hint.as_deref(), Some("[file]"));
        assert!(!spec.descriptor().model_invocable);
    }
}
