//! What decided a resolved posture value.

use super::Source;

/// What decided the resolved `mode` or `sandbox`.
///
/// Recorded where the value is decided, not reconstructed by a reader: the
/// startup note and `doctor` print this one fact, so they cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SettingProvenance {
    /// Nothing selected a value; the built-in default applies.
    #[default]
    Default,
    /// A configuration layer outside any profile.
    Layer { source: Source },
    /// The active profile table inside a configuration layer.
    Profile {
        source: Source,
        name: String,
        selected_by: Option<Source>,
    },
    /// A host profile ceiling lowered the selected value.
    ProfileCeiling,
    /// A trusted interactive launch raised an unset mode to `auto`.
    TrustedInteractive,
    /// A human accepted unconfined Windows execution at startup.
    UnconfinedAcknowledgement,
}

impl SettingProvenance {
    /// The value set by the active profile table or, when it has none, by the
    /// layer itself.
    pub(super) fn layer(source: Source, profile: Option<(&str, Option<Source>)>) -> Self {
        match profile {
            Some((name, selected_by)) => Self::Profile {
                source,
                name: name.to_owned(),
                selected_by,
            },
            None => Self::Layer { source },
        }
    }
}

impl std::fmt::Display for SettingProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Layer { source } => write!(f, "{source}"),
            Self::Profile {
                source,
                name,
                selected_by,
            } => {
                write!(f, "{source} profile {name}")?;
                match selected_by {
                    Some(selector) => write!(f, " selected by {selector}"),
                    None => Ok(()),
                }
            }
            Self::ProfileCeiling => f.write_str("host profile ceiling"),
            Self::TrustedInteractive => f.write_str("trusted interactive default"),
            Self::UnconfinedAcknowledgement => f.write_str("unconfined acknowledgement"),
        }
    }
}
