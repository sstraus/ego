//! The config schema. Every layer is `deny_unknown_fields`
//! (`CLI.md` §4.5) — an unrecognised key is a hard error, not a silent skip.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::error::ConfigError;
use super::source::{Source, Sourced};
use crate::ceiling::Budget;
use crate::generator::types::ThinkingLevel;
use crate::permit::Confinement;
use crate::permit::rule::Rule;

/// A model reference in `provider/name` form, e.g. `anthropic/claude-sonnet-5`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelRef(pub String);

/// Permission mode — `ARCHITECTURE.md` §3.4 axis 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Plan,
    Default,
    Auto,
    Edits,
    Yolo,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Plan => "plan",
            Mode::Default => "default",
            Mode::Auto => "auto",
            Mode::Edits => "edits",
            Mode::Yolo => "yolo",
        }
    }
}

impl std::fmt::Display for Mode {
    /// The spelling the operator types, so a refusal that names the mode names
    /// something they can go and change.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Deserialize the optional mode through one path for every config layer.
fn mode<'de, D>(deserializer: D) -> Result<Option<Mode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mode = Option::<Mode>::deserialize(deserializer)?;
    Ok(mode)
}

/// Permission rules (e.g. `"exec(cargo *)"`), split into allow and deny lists.
///
/// **Parsed here, not carried as text.** A `Vec<String>` would mean the rule an
/// operator wrote is validated at the moment it is first *used*, which is
/// mid-run, against one call, with the rest of the file already in force. It
/// also invites a second parser: whoever consumes the strings has to turn them
/// into something, and `Rule::load` being the only way in is what stops the
/// config path and the frontmatter path from disagreeing about what one string
/// means.
///
/// So a rule that is not a rule is a config error with a file and a line, and a
/// rule this build cannot enforce is refused by name at the same moment —
/// `2026-08-21-a-rule-that-means-exactly-one-thing.md` §6a.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermCfg {
    pub judge: Option<ModelRef>,
    #[serde(default, deserialize_with = "rules")]
    pub allow: Vec<Rule>,
    #[serde(default, deserialize_with = "rules")]
    pub deny: Vec<Rule>,
}

/// Both lists, through the one parser.
///
/// The error carries `Rule::load`'s own message — which names the head, offers
/// the effect when the operator wrote a tool, and says what is missing when the
/// head has no operations — wrapped in serde's location so it arrives with the
/// file and line attached.
fn rules<'de, D>(deserializer: D) -> Result<Vec<Rule>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)?
        .iter()
        .map(|raw| Rule::load(raw).map_err(serde::de::Error::custom))
        .collect()
}

/// The overrides a `[profile.NAME]` table may apply on top of the layer that
/// defines it (`CLI.md` §7, `--profile NAME`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileLayer {
    pub model: Option<ModelRef>,
    pub thinking: Option<ThinkingLevel>,
    #[serde(default, deserialize_with = "mode")]
    pub mode: Option<Mode>,
    pub sandbox: Option<Confinement>,
    pub permissions: Option<PermCfg>,
    /// A spend ceiling, in the same grammar `--budget` takes
    /// (`crate::ceiling::Budget`).
    pub budget: Option<Budget>,
}

/// A persisted status-line segment name.
///
/// Custom render values exist at the live UI boundary, but persisted names
/// must be supplied by ego itself so a typo cannot silently become an empty
/// row on every launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusSegmentName {
    Model,
    Effort,
    Mode,
    Context,
    Cost,
    Queue,
    Drafts,
    Interrupt,
    Cwd,
    Git,
}

impl Serialize for StatusSegmentName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StatusSegmentName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let name = String::deserialize(deserializer)?;
        Self::from_name(&name).ok_or_else(|| {
            serde::de::Error::unknown_variant(&name, StatusSegmentName::NAMES)
        })
    }
}

pub(super) fn validate_status_segment_names(names: &[String]) -> Result<(), String> {
    let segments = names
        .iter()
        .map(|name| {
            StatusSegmentName::from_name(name)
                .ok_or_else(|| format!("unknown status-line segment `{name}`"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    validate_unique_status_segments(&segments)
}

fn validate_unique_status_segments(segments: &[StatusSegmentName]) -> Result<(), String> {
    let mut unique = Vec::with_capacity(segments.len());
    for segment in segments {
        if unique.contains(segment) {
            return Err(format!(
                "duplicate status-line segment `{}`",
                segment.as_str()
            ));
        }
        unique.push(*segment);
    }
    Ok(())
}

impl StatusSegmentName {
    /// Every accepted persisted spelling.
    pub const NAMES: &[&str] = &[
        "model",
        "effort",
        "mode",
        "context",
        "cost",
        "queue",
        "drafts",
        "interrupt",
        "cwd",
        "git",
    ];

    /// Parse one persisted built-in name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "model" => Some(Self::Model),
            "effort" => Some(Self::Effort),
            "mode" => Some(Self::Mode),
            "context" => Some(Self::Context),
            "cost" => Some(Self::Cost),
            "queue" => Some(Self::Queue),
            "drafts" => Some(Self::Drafts),
            "interrupt" => Some(Self::Interrupt),
            "cwd" => Some(Self::Cwd),
            "git" => Some(Self::Git),
            _ => None,
        }
    }

    /// Return the canonical persisted spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Effort => "effort",
            Self::Mode => "mode",
            Self::Context => "context",
            Self::Cost => "cost",
            Self::Queue => "queue",
            Self::Drafts => "drafts",
            Self::Interrupt => "interrupt",
            Self::Cwd => "cwd",
            Self::Git => "git",
        }
    }
}

/// The strict `[ui.statusline]` schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatuslineLayer {
    #[serde(default, deserialize_with = "status_segments")]
    pub segments: Option<Vec<StatusSegmentName>>,
}

fn status_segments<'de, D>(deserializer: D) -> Result<Option<Vec<StatusSegmentName>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let segments = Option::<Vec<StatusSegmentName>>::deserialize(deserializer)?;
    if let Some(segments) = &segments {
        validate_unique_status_segments(segments).map_err(serde::de::Error::custom)?;
    }
    Ok(segments)
}

/// The strict `[ui]` schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiLayer {
    pub statusline: Option<StatuslineLayer>,
}

/// Non-secret authentication overrides for one provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthProviderLayer {
    /// Program invoked with `login` or `refresh`; its stdout is the secret.
    pub credential_command: Option<String>,
    /// Profile identity headers. Credential and wire headers are forbidden.
    #[serde(default, deserialize_with = "auth_headers")]
    pub headers: HashMap<String, String>,
}

fn auth_headers<'de, D>(deserializer: D) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let headers = HashMap::<String, String>::deserialize(deserializer)?;
    for name in headers.keys() {
        if [
            "authorization",
            "x-api-key",
            "content-type",
            "accept",
            "openai-beta",
            "chatgpt-account-id",
        ]
        .iter()
        .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
        {
            return Err(serde::de::Error::custom(format!(
                "forbidden authentication header `{name}`"
            )));
        }
    }
    Ok(headers)
}

/// One config layer: a file, an env-derived table, or the flags table.
/// `resolve` merges a slice of these, tagged by [`Source`], into an
/// [`Effective`](super::Effective) config.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigLayer {
    pub model: Option<ModelRef>,
    /// Default reasoning effort. Absence contributes no precedence value.
    pub thinking: Option<ThinkingLevel>,
    #[serde(default, deserialize_with = "mode")]
    pub mode: Option<Mode>,
    pub sandbox: Option<Confinement>,
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
    /// Provider login commands and non-secret identity-header overlays.
    #[serde(default)]
    pub auth: HashMap<String, AuthProviderLayer>,
    /// Interactive presentation configuration, resolved like other fields.
    pub ui: Option<UiLayer>,
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
