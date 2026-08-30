//! The config schema. Every layer is `deny_unknown_fields`
//! (`CLI.md` §4.5) — an unrecognised key is a hard error, not a silent skip.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use url::{Host, Url};

use super::error::ConfigError;
use super::source::{Source, Sourced};
use crate::ceiling::Budget;
use crate::generator::types::ThinkingLevel;
use crate::permit::Confinement;
use crate::permit::rule::Rule;

/// A model reference in `provider/name` form, e.g. `anthropic/claude-sonnet-5`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelRef(pub String);

/// Permission mode — `ARCHITECTURE.md` §3.4 axis 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
    /// CI-only durable session root. Other profile names are rejected while
    /// parsing the containing layer, even when they are inactive.
    pub session_dir: Option<std::path::PathBuf>,
    /// CI-only sanitized artifact root.
    pub artifact_dir: Option<std::path::PathBuf>,
    /// Complete MCP entries contributed by this profile. Entries replace the
    /// same name from the layer's base table; their fields never merge.
    #[serde(default)]
    pub mcp: McpLayer,
}

/// A stable MCP server identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McpServerName(String);

impl FromStr for McpServerName {
    type Err = McpValueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let valid = (1..=64).contains(&value.len())
            && value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && value.bytes().skip(1).all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            });
        if !valid {
            return Err(McpValueError::new(
                "MCP server name must match [a-z0-9][a-z0-9._-]{0,63}",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl McpServerName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for McpServerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for McpServerName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// One inherited environment variable name. Values remain outside config.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McpEnvironmentName(String);

impl FromStr for McpEnvironmentName {
    type Err = McpValueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut bytes = value.bytes();
        let valid = bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if !valid {
            return Err(McpValueError::new(
                "MCP environment name must match [A-Za-z_][A-Za-z0-9_]*",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl McpEnvironmentName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for McpEnvironmentName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for McpEnvironmentName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A semantic MCP value failed lexical or cross-field validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpValueError {
    message: String,
}

impl McpValueError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for McpValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for McpValueError {}

/// MCP entries contributed by one base layer or profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpLayer {
    #[serde(default)]
    pub servers: BTreeMap<McpServerName, McpLayerEntry>,
}

/// One complete named layer entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpLayerEntry {
    Server(McpServerConfig),
    Tombstone,
}

/// One current MCP transport configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerConfig {
    Stdio(McpStdioConfig),
    Http(McpHttpConfig),
}

impl McpServerConfig {
    pub fn stdio(
        command: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        inherit_env: Vec<McpEnvironmentName>,
    ) -> Result<Self, McpValueError> {
        McpStdioConfig::new(command, args, cwd, inherit_env).map(Self::Stdio)
    }

    pub fn http(
        url: Url,
        auth: McpHttpAuth,
        allow_insecure_loopback: bool,
    ) -> Result<Self, McpValueError> {
        McpHttpConfig::new(url, auth, allow_insecure_loopback).map(Self::Http)
    }

    pub fn validate(&self) -> Result<(), McpValueError> {
        match self {
            Self::Stdio(config) => config.validate(),
            Self::Http(config) => config.validate(),
        }
    }
}

/// A child-process MCP server. Environment values are inherited only when
/// execution begins and never become part of this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStdioConfig {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub inherit_env: Vec<McpEnvironmentName>,
}

impl McpStdioConfig {
    pub fn new(
        command: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        inherit_env: Vec<McpEnvironmentName>,
    ) -> Result<Self, McpValueError> {
        let config = Self {
            command,
            args,
            cwd,
            inherit_env,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), McpValueError> {
        if self.command.is_empty() {
            return Err(McpValueError::new("MCP stdio command must not be empty"));
        }
        let mut seen = Vec::<String>::with_capacity(self.inherit_env.len());
        for name in &self.inherit_env {
            let folded = name.0.to_ascii_lowercase();
            if seen.contains(&folded) {
                return Err(McpValueError::new(format!(
                    "duplicate MCP environment name `{name}` ignoring case"
                )));
            }
            seen.push(folded);
        }
        Ok(())
    }
}

/// Authentication behavior for a stateless HTTP MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpHttpAuth {
    #[default]
    None,
    Oauth,
}

/// A parsed absolute URL that preserves the operator's exact non-secret text
/// for diagnostics and comment-preserving configuration publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHttpUrl {
    text: String,
    parsed: Url,
}

impl McpHttpUrl {
    pub fn as_url(&self) -> &Url {
        &self.parsed
    }
}

impl From<Url> for McpHttpUrl {
    fn from(parsed: Url) -> Self {
        Self {
            text: parsed.to_string(),
            parsed,
        }
    }
}

impl FromStr for McpHttpUrl {
    type Err = McpValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let parsed = Url::parse(text)
            .map_err(|error| McpValueError::new(format!("invalid MCP HTTP URL: {error}")))?;
        Ok(Self {
            text: text.to_owned(),
            parsed,
        })
    }
}

impl std::fmt::Display for McpHttpUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for McpHttpUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A stateless HTTP MCP server after URL-policy validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHttpConfig {
    pub url: McpHttpUrl,
    pub auth: McpHttpAuth,
    pub allow_insecure_loopback: bool,
}

impl McpHttpConfig {
    pub fn new(
        url: impl Into<McpHttpUrl>,
        auth: McpHttpAuth,
        allow_insecure_loopback: bool,
    ) -> Result<Self, McpValueError> {
        let config = Self {
            url: url.into(),
            auth,
            allow_insecure_loopback,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), McpValueError> {
        if authority_has_userinfo(&self.url.text)
            || !self.url.parsed.username().is_empty()
            || self.url.parsed.password().is_some()
        {
            return Err(McpValueError::new("MCP HTTP URL must not contain userinfo"));
        }
        if self.url.parsed.fragment().is_some() {
            return Err(McpValueError::new(
                "MCP HTTP URL must not contain a fragment",
            ));
        }
        if self.url.parsed.host().is_none() {
            return Err(McpValueError::new(
                "MCP HTTP URL must have an absolute host",
            ));
        }

        match self.url.parsed.scheme() {
            "https" if self.allow_insecure_loopback => Err(McpValueError::new(
                "allow_insecure_loopback is invalid for HTTPS",
            )),
            "https" => Ok(()),
            "http" if !self.allow_insecure_loopback => Err(McpValueError::new(
                "plaintext HTTP requires allow_insecure_loopback = true",
            )),
            "http" if !is_exact_loopback(&self.url) => Err(McpValueError::new(
                "plaintext HTTP requires exact localhost or an IP-literal loopback host",
            )),
            "http" => Ok(()),
            _ => Err(McpValueError::new(
                "MCP HTTP URL scheme must be http or https",
            )),
        }
    }
}

fn authority_has_userinfo(text: &str) -> bool {
    text.split_once("://")
        .map(|(_, remainder)| {
            remainder
                .split(['/', '?', '#'])
                .next()
                .is_some_and(|authority| authority.contains('@'))
        })
        .unwrap_or(false)
}

fn is_exact_loopback(url: &McpHttpUrl) -> bool {
    match url.parsed.host() {
        Some(Host::Domain(host)) => host == "localhost",
        Some(Host::Ipv4(address)) => IpAddr::V4(address).is_loopback(),
        Some(Host::Ipv6(address)) => IpAddr::V6(address).is_loopback(),
        None => false,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpStdioWire {
    transport: McpStdioTransport,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    inherit_env: Vec<McpEnvironmentName>,
}

#[derive(Deserialize)]
enum McpStdioTransport {
    #[serde(rename = "stdio")]
    Stdio,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpHttpWire {
    transport: McpHttpTransport,
    url: McpHttpUrl,
    #[serde(default)]
    auth: McpHttpAuth,
    #[serde(default)]
    allow_insecure_loopback: bool,
}

#[derive(Deserialize)]
enum McpHttpTransport {
    #[serde(rename = "http")]
    Http,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpTombstoneWire {
    removed: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum McpEntryWire {
    Stdio(McpStdioWire),
    Http(McpHttpWire),
    Tombstone(McpTombstoneWire),
}

impl McpEntryWire {
    fn into_entry(self) -> Result<McpLayerEntry, McpValueError> {
        match self {
            Self::Stdio(wire) => {
                let _ = wire.transport;
                McpStdioConfig::new(wire.command, wire.args, wire.cwd, wire.inherit_env)
                    .map(McpServerConfig::Stdio)
                    .map(McpLayerEntry::Server)
            }
            Self::Http(wire) => {
                let _ = wire.transport;
                McpHttpConfig::new(wire.url, wire.auth, wire.allow_insecure_loopback)
                    .map(McpServerConfig::Http)
                    .map(McpLayerEntry::Server)
            }
            Self::Tombstone(wire) if wire.removed => Ok(McpLayerEntry::Tombstone),
            Self::Tombstone(_) => Err(McpValueError::new("MCP tombstone requires removed = true")),
        }
    }
}

impl<'de> Deserialize<'de> for McpLayerEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        McpEntryWire::deserialize(deserializer)?
            .into_entry()
            .map_err(serde::de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match McpEntryWire::deserialize(deserializer)?
            .into_entry()
            .map_err(serde::de::Error::custom)?
        {
            McpLayerEntry::Server(server) => Ok(server),
            McpLayerEntry::Tombstone => Err(serde::de::Error::custom(
                "an invocation-scoped MCP server cannot be a tombstone",
            )),
        }
    }
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
        Self::from_name(&name)
            .ok_or_else(|| serde::de::Error::unknown_variant(&name, StatusSegmentName::NAMES))
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
    /// Name-keyed MCP entries in this layer.
    #[serde(default)]
    pub mcp: McpLayer,
}

impl ConfigLayer {
    /// Parses one layer from injected TOML text, tagging the result with the
    /// source it came from. No file I/O happens here — the text is provided
    /// by the caller (`ego-tools` reads the file; this stays pure).
    pub fn parse(source: Source, text: &str) -> Result<Sourced<ConfigLayer>, ConfigError> {
        let value: ConfigLayer =
            toml::from_str(text).map_err(|err| classify_parse_error(source, &err))?;
        if let Some((name, _)) = value.profile.iter().find(|(name, profile)| {
            name.as_str() != "ci"
                && (profile.session_dir.is_some() || profile.artifact_dir.is_some())
        }) {
            return Err(ConfigError::Invalid {
                source,
                message: format!(
                    "profile `{name}` cannot set `session_dir` or `artifact_dir`; those keys belong only to profile `ci`"
                ),
            });
        }
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
