//! Generation of the private macOS Seatbelt profile used for one run.
//!
//! SBPL is data here. Every path enters the profile through [`path_literal`];
//! callers cannot contribute a fragment of the profile language.

use std::{collections::BTreeSet, error::Error, fmt, path::Path};

use ego_core::permit::Confinement;

/// Inputs owned by the execution backend, rather than by a model call.
pub(super) struct ProfileInput<'a> {
    pub(super) level: Confinement,
    pub(super) offline: bool,
    pub(super) workspace: &'a Path,
    pub(super) private_scratch: &'a Path,
    pub(super) runtime_roots: &'a [&'a Path],
    /// Paths whose ancestors are made statable: a profile's exec paths, which
    /// are already part of `runtime_roots`, and the workspace beside them.
    pub(super) exec_paths: &'a [&'a Path],
}

#[derive(Debug)]
pub(super) enum ProfileError {
    RelativePath(String),
    UnrepresentablePath(String),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RelativePath(path) => write!(f, "Seatbelt root is not absolute: {path}"),
            Self::UnrepresentablePath(path) => {
                write!(f, "Seatbelt root cannot be represented safely: {path}")
            }
        }
    }
}

impl Error for ProfileError {}

/// Render one complete profile for the exact effective run policy.
pub(super) fn render(input: &ProfileInput<'_>) -> Result<String, ProfileError> {
    let workspace = path_literal(input.workspace)?;
    let scratch = path_literal(input.private_scratch)?;
    let mut readable = input
        .runtime_roots
        .iter()
        .copied()
        .map(path_literal)
        .collect::<Result<BTreeSet<_>, _>>()?;
    readable.insert(workspace.clone());

    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         (import \"system.sb\")\n\
         (allow process-fork)\n",
    );

    match input.level {
        Confinement::Off => profile.push_str("(allow process-exec*)\n(allow file*)\n"),
        Confinement::Ro | Confinement::Workspace => {
            profile.push_str("(allow process-exec*\n");
            for root in &readable {
                profile.push_str("  (subpath ");
                profile.push_str(root);
                profile.push_str(")\n");
            }
            profile.push_str(")\n");

            // A writable root the child cannot stat is not usable: `touch`,
            // `cp`, `rm`, `mv` and `ln` all read metadata before they mutate,
            // so a write-only scratch fails with `Operation not permitted`.
            // Scratch joins the read set with the other writable root, and
            // never the exec set above: ego does not let a child run a program
            // it has just written into its own scratch.
            if matches!(input.level, Confinement::Workspace) {
                readable.insert(scratch.clone());
            }

            profile.push_str("(allow file-read* file-test-existence\n");
            for root in readable {
                profile.push_str("  (subpath ");
                profile.push_str(&root);
                profile.push_str(")\n");
            }
            profile.push_str(")\n");

            // `realpath`, command lookup and `mkdir -p` lstat each ancestor of
            // a path they use, so a program under an exec path needs them
            // statable. Metadata only: no listing and no file contents.
            let ancestors = input
                .exec_paths
                .iter()
                .flat_map(|path| path.ancestors().skip(1))
                .filter(|ancestor| *ancestor != Path::new("/"))
                .map(path_literal)
                .collect::<Result<BTreeSet<_>, _>>()?;
            if !ancestors.is_empty() {
                profile.push_str("(allow file-read-metadata\n");
                for ancestor in ancestors {
                    profile.push_str("  (literal ");
                    profile.push_str(&ancestor);
                    profile.push_str(")\n");
                }
                profile.push_str(")\n");
            }

            match input.level {
                Confinement::Ro => profile.push_str("(deny file-write*)\n"),
                Confinement::Workspace => {
                    // `system.sb` contains a small platform runtime baseline.
                    // Narrow its write grants back to the two policy roots.
                    profile.push_str("(deny file-write*\n  (require-not (require-any\n");
                    for root in [&workspace, &scratch] {
                        profile.push_str("    (subpath ");
                        profile.push_str(root);
                        profile.push_str(")\n");
                    }
                    profile.push_str("  )))\n");
                    profile.push_str("(allow file-write*\n");
                    for root in [&workspace, &scratch] {
                        profile.push_str("  (subpath ");
                        profile.push_str(root);
                        profile.push_str(")\n");
                    }
                    profile.push_str(")\n");
                }
                Confinement::Off => unreachable!("handled above"),
            }
        }
    }

    if input.offline {
        profile.push_str("(deny network*)\n");
    } else {
        profile.push_str("(allow network*)\n");
    }

    Ok(profile)
}

/// Encode an absolute host path as one SBPL string literal.
///
/// SBPL has no stable public grammar. We therefore use only its installed
/// quoted-string subset, escape its two structural characters, and reject
/// control bytes instead of guessing at escape spellings. Escaping `"` keeps
/// a closing parenthesis in the string, where it has no syntactic meaning.
fn path_literal(path: &Path) -> Result<String, ProfileError> {
    if !path.is_absolute() {
        return Err(ProfileError::RelativePath(path_label(path)));
    }
    let raw = path
        .to_str()
        .ok_or_else(|| ProfileError::UnrepresentablePath(path_label(path)))?;
    if raw.chars().any(char::is_control) {
        return Err(ProfileError::UnrepresentablePath(path_label(path)));
    }

    let mut encoded = String::with_capacity(raw.len() + 2);
    encoded.push('"');
    for ch in raw.chars() {
        match ch {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            _ => encoded.push(ch),
        }
    }
    encoded.push('"');
    Ok(encoded)
}

fn path_label(path: &Path) -> String {
    format!("{:?}", path.as_os_str())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Confinement, ProfileInput, render};

    fn input(level: Confinement) -> ProfileInput<'static> {
        static RUNTIME: &[&Path] = &[];
        ProfileInput {
            level,
            offline: false,
            workspace: Path::new("/work"),
            private_scratch: Path::new("/scratch"),
            runtime_roots: RUNTIME,
            exec_paths: RUNTIME,
        }
    }

    #[test]
    fn workspace_grants_read_and_existence_on_both_writable_roots() {
        assert_eq!(
            render(&input(Confinement::Workspace)).expect("workspace profile"),
            concat!(
                "(version 1)\n",
                "(deny default)\n",
                "(import \"system.sb\")\n",
                "(allow process-fork)\n",
                "(allow process-exec*\n",
                "  (subpath \"/work\")\n",
                ")\n",
                "(allow file-read* file-test-existence\n",
                "  (subpath \"/scratch\")\n",
                "  (subpath \"/work\")\n",
                ")\n",
                "(deny file-write*\n",
                "  (require-not (require-any\n",
                "    (subpath \"/work\")\n",
                "    (subpath \"/scratch\")\n",
                "  )))\n",
                "(allow file-write*\n",
                "  (subpath \"/work\")\n",
                "  (subpath \"/scratch\")\n",
                ")\n",
                "(allow network*)\n",
            )
        );
    }

    #[test]
    fn exec_path_ancestors_get_literal_metadata_grants_and_no_write() {
        let exec = [Path::new("/opt/tools/bin"), Path::new("/opt/other")];
        let profile = render(&ProfileInput {
            runtime_roots: &exec,
            exec_paths: &exec,
            ..input(Confinement::Workspace)
        })
        .expect("profile");
        assert!(
            profile.contains(concat!(
                "(allow file-read-metadata\n",
                "  (literal \"/opt\")\n",
                "  (literal \"/opt/tools\")\n",
                ")\n"
            )),
            "{profile}"
        );
        assert!(!profile.contains("(subpath \"/opt\")"), "{profile}");
        assert!(
            profile.contains("(subpath \"/opt/tools/bin\")"),
            "{profile}"
        );
    }

    #[test]
    fn read_only_keeps_the_private_scratch_out_of_every_rule() {
        // `ro` mutates nothing, so scratch is not a root the child needs, and
        // an unused read grant would still be reach the level does not promise.
        let profile = render(&input(Confinement::Ro)).expect("ro profile");
        assert!(!profile.contains("/scratch"), "{profile}");
    }
}
