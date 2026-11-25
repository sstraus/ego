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
    // A command is read by the same field parsers as a skill, so a file written for
    // either loads the same way: unknown fields are ignored, and the tool and model
    // fields only narrow or select, as they do for a skill. The command stays
    // user-invocable and is never offered to the model.
    crate::skill::validate_field_names(&fields)?;
    let description = crate::skill::take_optional_string(&mut fields, "description")?
        .unwrap_or_else(|| format!("Run {name}"));
    let argument_hint = crate::skill::take_argument_hint(&mut fields)?;
    let allowed_tools = crate::skill::take_tools(&mut fields, "allowed-tools")?;
    let disallowed_tools =
        crate::skill::take_tools(&mut fields, "disallowed-tools")?.unwrap_or_default();
    let model = crate::skill::take_optional_string(&mut fields, "model")?
        .filter(|model| !model.eq_ignore_ascii_case("inherit"))
        .map(ego_core::config::ModelRef);
    let effort = crate::skill::take_optional_string(&mut fields, "effort")?
        .map(|value| value.to_lowercase());
    if let Some(value) = effort.as_deref()
        && ego_core::skill::parse_skill_effort(value).is_none()
    {
        return Err(format!("field `effort` has unsupported value `{value}`"));
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
    let mut spec = SkillSpec::new(descriptor, body.as_bytes().to_vec())
        .map(|spec| {
            spec.with_metadata(BTreeMap::from([("ego-command".into(), "true".into())]))
                .with_model_selection(model, effort)
                .with_disallowed_tools(disallowed_tools)
        })
        .map_err(|e| e.to_string())?;
    if let Some(tools) = allowed_tools {
        spec = spec.with_tool_ceiling(tools);
    }
    Ok(spec)
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

    // Catches: builtin case folding shadows exact custom names or folds other custom names.
    #[test]
    fn mixed_case_builtins_preserve_exact_custom_command_names() {
        use crate::repl::actions::{ActionId, ActionRegistry, Dispatch, InputOrigin};

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let commands = workspace.join(".claude/commands");
        fs::create_dir_all(&commands).unwrap();
        fs::write(commands.join("Model.md"), "Custom model $1").unwrap();
        fs::write(commands.join("Review.md"), "Custom review $1").unwrap();
        let discovery = crate::skill::SkillDiscovery::for_workspace(&workspace, true).unwrap();
        let actions = ActionRegistry::final_surface();
        let names = actions
            .names()
            .into_iter()
            .chain(actions.aliases())
            .collect::<Vec<_>>();
        let catalogue = crate::skill::SkillCatalogue::discover(discovery, &names).unwrap();
        actions.bind_skill_view(catalogue.view()).unwrap();

        for input in ["/Model MixedCase", "/Review MixedCase"] {
            assert!(
                matches!(
                    actions.classify(actions.submit(input, InputOrigin::Typed)),
                    Dispatch::Skill(_)
                ),
                "{input:?}"
            );
        }
        for input in ["/model", "/MODEL"] {
            assert_eq!(
                actions.classify(actions.submit(input, InputOrigin::Typed)),
                Dispatch::Action(ActionId::Model),
                "{input:?}"
            );
        }
        assert!(matches!(
            actions.classify(actions.submit("/review MixedCase", InputOrigin::Typed)),
            Dispatch::UnknownSlash(_)
        ));
    }

    // Catches: a command file written for the reference format is refused for a field
    // ego reads in a skill, or its tool ceiling and model selection are dropped.
    #[test]
    fn a_command_reads_the_skill_fields_and_stays_user_only() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("review.md");
        fs::write(
            &path,
            "---\nmodel: vendor/model\neffort: High\nallowed-tools: read grep\ndisallowed-tools: Bash\nargument-hint: [file]\nagent: build\nfuture-field: 1\n---\nReview $1",
        )
        .unwrap();
        let spec = load(SkillSourceAuthority::User, &path, "review").unwrap();
        assert_eq!(spec.model(), Some("vendor/model"));
        assert_eq!(spec.effort(), Some("high"));
        assert_eq!(
            spec.allowed_tools().unwrap().iter().collect::<Vec<_>>(),
            ["grep", "read"]
        );
        assert!(spec.disallowed_tools().contains("exec"));
        assert_eq!(spec.descriptor().argument_hint.as_deref(), Some("[file]"));
        assert!(spec.descriptor().user_invocable);
        assert!(!spec.descriptor().model_invocable);
        // A foreign tool name still narrows nothing by accident: it is refused.
        fs::write(&path, "---\nallowed-tools: Bash\n---\nReview").unwrap();
        assert!(load(SkillSourceAuthority::User, &path, "review").is_err());
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
