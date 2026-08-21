//! The config schema. Every layer is `deny_unknown_fields`
//! (`CLI.md` §4.5) — an unrecognised key is a hard error, not a silent skip.

use std::collections::HashMap;

use serde::Deserialize;

use super::error::ConfigError;
use super::source::{Source, Sourced};
use crate::ceiling::Budget;

/// A model reference in `provider/name` form, e.g. `anthropic/claude-sonnet-5`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct ModelRef(pub String);

/// Permission mode — `ARCHITECTURE.md` §3.4 axis 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Plan,
    Default,
    Auto,
    Edits,
    Yolo,
}

/// Permission rule identifiers (e.g. `"bash(cargo *)"`), split into allow and
/// deny lists. The full rule grammar with stable IDs (`ARCHITECTURE.md` §3.4
/// axis 2) is a later milestone; this is the minimal shape strict config and
/// the system ceiling need today.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermCfg {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

/// The overrides a `[profile.NAME]` table may apply on top of the layer that
/// defines it (`CLI.md` §7, `--profile NAME`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileLayer {
    pub model: Option<ModelRef>,
    pub mode: Option<Mode>,
    pub permissions: Option<PermCfg>,
    /// A spend ceiling, in the same grammar `--budget` takes
    /// (`crate::ceiling::Budget`).
    pub budget: Option<Budget>,
}

/// One config layer: a file, an env-derived table, or the flags table.
/// `resolve` merges a slice of these, tagged by [`Source`], into an
/// [`Effective`](super::Effective) config.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigLayer {
    pub model: Option<ModelRef>,
    pub mode: Option<Mode>,
    pub permissions: Option<PermCfg>,
    /// A spend ceiling, in the same grammar `--budget` takes
    /// (`crate::ceiling::Budget`).
    ///
    /// `CLI.md:296-299` promises an administrator this key on
    /// `/etc/ego/config.toml` and calls the system layer "the only source of
    /// ceilings". This layer is `deny_unknown_fields`, so until the key
    /// existed the promise could not be kept by anybody: writing it produced
    /// `ConfigError::UnknownKey`.
    pub budget: Option<Budget>,
    /// Mirrors `--bare`: forces `Posture::Bare` even in a trusted workspace.
    pub bare: Option<bool>,
    /// `[profile.NAME]` sections, selected with `--profile NAME` /
    /// `EGO_PROFILE` (`CLI.md` §7). Keyed by profile name.
    #[serde(default)]
    pub profile: HashMap<String, ProfileLayer>,
}

impl ConfigLayer {
    /// Parses one layer from injected TOML text, tagging the result with the
    /// source it came from. No file I/O happens here — the text is provided
    /// by the caller (`ego-tools` reads the file; this stays pure).
    pub fn parse(source: Source, text: &str) -> Result<Sourced<ConfigLayer>, ConfigError> {
        let value: ConfigLayer =
            toml::from_str(text).map_err(|err| classify_parse_error(source, &err))?;
        Ok(Sourced { source, value })
    }
}

/// `toml`'s `deny_unknown_fields` diagnostic reads
/// `unknown field \`key\`, expected ...` or
/// `unknown field \`key\`, there are no fields` regardless of nesting depth
/// (a nested table's unknown field is reported by leaf name only, e.g.
/// `permissions.bogus` and `profile.ci.bogus` both surface as `bogus`).
///
/// The key itself may contain a backtick (TOML allows quoted string keys),
/// so the closing backtick can't be found with a naive first-match: anchor
/// on the trailing `` `, expected `` / `` `, there are no fields `` marker
/// instead, taking its *last* occurrence.
fn classify_parse_error(source: Source, err: &toml::de::Error) -> ConfigError {
    let message = err.message();
    if let Some(rest) = message.strip_prefix("unknown field `") {
        let end = ["`, expected", "`, there are no fields"]
            .iter()
            .find_map(|marker| rest.rfind(marker));
        if let Some(end) = end {
            return ConfigError::UnknownKey {
                key: rest[..end].to_string(),
                source,
            };
        }
    }
    ConfigError::Invalid {
        source,
        message: message.to_string(),
    }
}
