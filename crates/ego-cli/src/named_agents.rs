//! Trusted, read-only task definitions using the ordinary agent-file envelope.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ego_core::config::Trust;
use ego_core::permit::CanonPath;
use ego_core::turn::NamedAgent;

use crate::cli::UsageError;

pub(crate) fn discover(workspace: &Path, trust: Trust) -> Result<Vec<NamedAgent>, UsageError> {
    let Some(ego_home) = crate::config::ego_home() else {
        return Ok(Vec::new());
    };
    // Do not interpret foreign CLI agent formats as ego policy.
    let mut roots = Vec::new();
    if trust == Trust::Trusted {
        roots.push(workspace.join(".ego/agents"));
    }
    roots.push(ego_home.join("agents"));
    roots.push(PathBuf::from("/etc/ego/agents"));
    discover_roots(&roots)
}

fn discover_roots(roots: &[PathBuf]) -> Result<Vec<NamedAgent>, UsageError> {
    let mut names = BTreeSet::new();
    let mut agents = Vec::new();
    for root in roots {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(UsageError(format!(
                    "agent catalogue {}: {error}",
                    root.display()
                )));
            }
        };
        let canonical = root
            .canonicalize()
            .map_err(|error| UsageError(error.to_string()))?;
        // A project-local link must not silently import definitions outside its scope.
        if [Some(root.as_path()), root.parent()]
            .into_iter()
            .flatten()
            .any(|path| {
                std::fs::symlink_metadata(path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
            })
        {
            return Err(UsageError(format!(
                "agent catalogue {} must not traverse symlinks",
                root.display()
            )));
        }
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| UsageError(error.to_string()))?;
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "md")
            {
                continue;
            }
            if !entry
                .file_type()
                .map_err(|error| UsageError(error.to_string()))?
                .is_file()
            {
                return Err(UsageError(format!(
                    "agent definition {} must be a regular file",
                    entry.path().display()
                )));
            }
            files.push(entry.path());
            if files.len() > 256 {
                return Err(UsageError(
                    "agent catalogue exceeds 256 definitions per root".into(),
                ));
            }
        }
        files.sort();
        for path in files {
            let name = path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if name.is_empty()
                || name.len() > 80
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                return Err(UsageError(format!(
                    "invalid named agent filename {}",
                    path.display()
                )));
            }
            if !names.insert(name.to_owned()) {
                continue;
            }
            let file = crate::agent_file::load_definition(&path, &[])?;
            if file.path().parent() != Some(canonical.as_path()) {
                return Err(UsageError(format!(
                    "named agent {name} resolves outside its catalogue"
                )));
            }
            let layer = file.layer()?.value;
            let permissions = layer.permissions.unwrap_or_default();
            if !permissions.allow.is_empty() || file.schema().is_some() {
                return Err(UsageError(format!(
                    "named agent {name} cannot grant allow rules or set an output schema"
                )));
            }
            let roots = file
                .roots
                .as_ref()
                .map(|roots| {
                    roots
                        .iter()
                        .map(|root| {
                            let path = Path::new(root);
                            if !path.is_absolute() {
                                return Err(UsageError(format!(
                                    "named agent {name} roots must be absolute"
                                )));
                            }
                            path.canonicalize()
                                .map(|path| CanonPath::new(&path))
                                .map_err(|error| {
                                    UsageError(format!("named agent {name} root: {error}"))
                                })
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?;
            agents.push(NamedAgent {
                name: name.to_owned(),
                prompt: file.body().to_owned(),
                model: layer.model,
                effort: layer.thinking,
                tools: file.tools.map(|tools| tools.into_iter().collect()),
                mode: layer.mode,
                deny: permissions.deny,
                sandbox: layer.sandbox,
                roots,
            });
        }
    }
    Ok(agents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_only_catalogue_reuses_strict_parser_and_keeps_tool_ceilings() {
        // Catches: discovery requiring a project file or dropping an empty tool ceiling.
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let user = root.as_path().join("user");
        std::fs::create_dir(&user).unwrap();
        std::fs::write(user.join("review.md"), "---\nversion: 1\nmodel: ollama/qwen3\ntools: []\nmode: plan\ndeny: [\"write(**)\"]\n---\nReview only.\n").unwrap();
        let agents = discover_roots(&[root.as_path().join("absent"), user]).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "review");
        assert_eq!(agents[0].prompt, "Review only.\n");
        assert_eq!(agents[0].model.as_ref().unwrap().0, "ollama/qwen3");
        assert!(agents[0].tools.as_ref().unwrap().is_empty());
        assert_eq!(agents[0].deny.len(), 1);
    }

    #[test]
    fn definition_accepts_a_valid_effort_and_refuses_an_unknown_one() {
        // Catches: `effort` refused as an unknown field, or an unknown level reaching a child.
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        std::fs::write(
            root.as_path().join("deep.md"),
            "---\nversion: 1\neffort: high\n---\nThink.",
        )
        .unwrap();
        let agents = discover_roots(&[root.as_path().to_path_buf()]).unwrap();
        assert_eq!(
            agents[0].effort,
            Some(ego_core::generator::types::ThinkingLevel::High)
        );
        std::fs::write(
            root.as_path().join("deep.md"),
            "---\nversion: 1\neffort: ludicrous\n---\nThink.",
        )
        .unwrap();
        assert!(discover_roots(&[root.as_path().to_path_buf()]).is_err());
    }

    #[test]
    fn higher_priority_definition_wins_without_merging_authority() {
        // Catches: a user duplicate widening a project definition's tool ceiling.
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let project = root.as_path().join("project");
        let user = root.as_path().join("user");
        for directory in [&project, &user] {
            std::fs::create_dir(directory).unwrap();
        }
        std::fs::write(
            project.join("review.md"),
            "---\nversion: 1\ntools: []\n---\nProject",
        )
        .unwrap();
        std::fs::write(
            user.join("review.md"),
            "---\nversion: 1\ntools: [exec]\n---\nUser",
        )
        .unwrap();
        let agents = discover_roots(&[project, user]).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].prompt, "Project");
        assert!(agents[0].tools.as_ref().unwrap().is_empty());
    }

    #[test]
    fn named_definitions_cannot_import_permission_grants() {
        // Catches: importing CLI allow rules as child authority.
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        std::fs::write(
            root.as_path().join("unsafe.md"),
            "---\nversion: 1\nallow: [\"write(**)\"]\n---\nWrite",
        )
        .unwrap();
        assert!(
            discover_roots(&[root.as_path().to_path_buf()])
                .unwrap_err()
                .0
                .contains("cannot grant")
        );
    }

    #[cfg(unix)]
    #[test]
    fn platform_ancestor_alias_does_not_reject_a_regular_catalogue() {
        // Catches: macOS /etc -> /private/etc making every system catalogue invalid.
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let actual = root.join("actual");
        let definitions = actual.join("ego/agents");
        std::fs::create_dir_all(&definitions).unwrap();
        std::fs::write(
            definitions.join("review.md"),
            "---\nversion: 1\n---\nReview",
        )
        .unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(actual, &alias).unwrap();
        assert_eq!(
            discover_roots(&[alias.join("ego/agents")]).unwrap().len(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_definition_cannot_escape_its_trusted_catalogue() {
        // Catches: following a definition symlink to an untrusted location.
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let definitions = root.as_path().join("agents");
        std::fs::create_dir(&definitions).unwrap();
        let outside = root.as_path().join("outside.md");
        std::fs::write(&outside, "---\nversion: 1\n---\nOutside").unwrap();
        std::os::unix::fs::symlink(outside, definitions.join("escape.md")).unwrap();
        assert!(
            discover_roots(&[definitions])
                .unwrap_err()
                .0
                .contains("regular file")
        );
    }
}
