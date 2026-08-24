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

            profile.push_str("(allow file-read* file-test-existence\n");
            for root in readable {
                profile.push_str("  (subpath ");
                profile.push_str(&root);
                profile.push_str(")\n");
            }
            profile.push_str(")\n");

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
