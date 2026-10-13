//! Closed vocabulary for user-scoped configuration management.

use std::borrow::{Borrow, Cow};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::layer::{Mode, ModelRef, StatusSegmentName, validate_unique_status_segments};
use super::mutation::UserConfigError;
use crate::ceiling::Budget;
use crate::generator::types::ThinkingLevel;
use crate::permit::Confinement;

/// A validated name selecting one `[profile.NAME]` table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileName(String);

impl ProfileName {
    /// Validate one profile name at the boundary where it enters the domain.
    pub fn parse(value: &str) -> Result<Self, UserConfigError> {
        if !(1..=64).contains(&value.len()) {
            return Err(UserConfigError::new(
                "profile name must contain between 1 and 64 UTF-8 bytes",
            ));
        }
        if value.chars().any(|character| character.is_ascii_control()) {
            return Err(UserConfigError::new(
                "profile name must not contain ASCII control characters",
            ));
        }
        if value
            .chars()
            .next()
            .is_some_and(|character| character.is_whitespace())
            || value
                .chars()
                .next_back()
                .is_some_and(|character| character.is_whitespace())
        {
            return Err(UserConfigError::new(
                "profile name must not have leading or trailing Unicode whitespace",
            ));
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProfileName {
    type Err = UserConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl AsRef<str> for ProfileName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for ProfileName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl Serialize for ProfileName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProfileName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// The sole user-owned configuration target for typed management mutations.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UserConfigTarget {
    Base,
    Profile(ProfileName),
}

impl UserConfigTarget {
    pub fn profile(&self) -> Option<&ProfileName> {
        match self {
            Self::Base => None,
            Self::Profile(profile) => Some(profile),
        }
    }
}

/// A field writable by the general configuration management service.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ManagedConfigKey {
    Model,
    Thinking,
    Mode,
    Sandbox,
    Roots,
    ExecPaths,
    AuthHeaders(String),
    AuthCredentialCommand(String),
    ExecEnv,
    ExecEnvAllowlist,
    Network,
    Budget,
    Bare,
    PermissionsJudge,
    StatuslineSegments,
    SessionDir,
    ArtifactDir,
}

impl ManagedConfigKey {
    const BASE: [Self; 13] = [
        Self::Model,
        Self::Thinking,
        Self::Mode,
        Self::Sandbox,
        Self::Budget,
        Self::Bare,
        Self::PermissionsJudge,
        Self::StatuslineSegments,
        Self::ExecEnv,
        Self::ExecEnvAllowlist,
        Self::Network,
        Self::Roots,
        Self::ExecPaths,
    ];
    const PROFILE: [Self; 11] = [
        Self::Model,
        Self::Thinking,
        Self::Mode,
        Self::Sandbox,
        Self::Budget,
        Self::PermissionsJudge,
        Self::ExecEnv,
        Self::ExecEnvAllowlist,
        Self::Network,
        Self::Roots,
        Self::ExecPaths,
    ];
    const CI_PROFILE: [Self; 13] = [
        Self::Model,
        Self::Thinking,
        Self::Mode,
        Self::Sandbox,
        Self::Budget,
        Self::PermissionsJudge,
        Self::SessionDir,
        Self::ArtifactDir,
        Self::ExecEnv,
        Self::ExecEnvAllowlist,
        Self::Network,
        Self::Roots,
        Self::ExecPaths,
    ];

    pub fn as_str(&self) -> Cow<'_, str> {
        Cow::Borrowed(match self {
            Self::Model => "model",
            Self::Thinking => "thinking",
            Self::Mode => "mode",
            Self::Sandbox => "sandbox",
            Self::ExecEnv => "exec.env",
            Self::ExecEnvAllowlist => "exec.env_allowlist",
            Self::Roots => "roots",
            Self::ExecPaths => "exec_paths",
            Self::AuthHeaders(provider) => return Cow::Owned(format!("auth.{provider}.headers")),
            Self::AuthCredentialCommand(provider) => {
                return Cow::Owned(format!("auth.{provider}.credential_command"));
            }
            Self::Network => "network",
            Self::Budget => "budget",
            Self::Bare => "bare",
            Self::PermissionsJudge => "permissions.judge",
            Self::StatuslineSegments => "ui.statusline.segments",
            Self::SessionDir => "session_dir",
            Self::ArtifactDir => "artifact_dir",
        })
    }

    /// Static keys available in `target`, in canonical order; auth keys name a provider.
    pub fn for_target(target: UserConfigTarget) -> &'static [Self] {
        match target {
            UserConfigTarget::Base => &Self::BASE,
            UserConfigTarget::Profile(profile) if profile.as_str() == "ci" => &Self::CI_PROFILE,
            UserConfigTarget::Profile(_) => &Self::PROFILE,
        }
    }

    /// Dynamic authentication keys are available only at the user base.
    pub fn available_in(&self, target: &UserConfigTarget) -> bool {
        if self.is_secret() {
            matches!(target, UserConfigTarget::Base)
        } else {
            Self::for_target(target.clone()).contains(self)
        }
    }

    pub fn is_secret(&self) -> bool {
        matches!(self, Self::AuthHeaders(_) | Self::AuthCredentialCommand(_))
    }

    /// Parse only the canonical spelling; aliases and arbitrary dotted keys fail.
    pub fn parse(value: &str) -> Result<Self, UserConfigError> {
        if let Some(auth) = value.strip_prefix("auth.")
            && let Some((provider, field)) = auth.rsplit_once('.')
            && !provider.is_empty()
            && !provider.chars().any(char::is_control)
        {
            match field {
                "headers" => return Ok(Self::AuthHeaders(provider.to_owned())),
                "credential_command" => {
                    return Ok(Self::AuthCredentialCommand(provider.to_owned()));
                }
                _ => {}
            }
        }
        match value {
            "exec_paths" => Ok(Self::ExecPaths),
            "model" => Ok(Self::Model),
            "thinking" => Ok(Self::Thinking),
            "mode" => Ok(Self::Mode),
            "sandbox" => Ok(Self::Sandbox),
            "exec.env" => Ok(Self::ExecEnv),
            "exec.env_allowlist" => Ok(Self::ExecEnvAllowlist),
            "roots" => Ok(Self::Roots),
            "network" => Ok(Self::Network),
            "budget" => Ok(Self::Budget),
            "bare" => Ok(Self::Bare),
            "permissions.judge" => Ok(Self::PermissionsJudge),
            "ui.statusline.segments" => Ok(Self::StatuslineSegments),
            "session_dir" => Ok(Self::SessionDir),
            "artifact_dir" => Ok(Self::ArtifactDir),
            _ => Err(UserConfigError::new(format!(
                "unknown managed configuration key `{value}`"
            ))),
        }
    }
}

impl fmt::Display for ManagedConfigKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl FromStr for ManagedConfigKey {
    type Err = UserConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// One strictly typed value accepted by a [`ManagedConfigKey`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedConfigValue {
    Model(ModelRef),
    Thinking(ThinkingLevel),
    Mode(Mode),
    Sandbox(Confinement),
    Roots(Vec<crate::permit::RootCfg>),
    ExecPaths(Vec<PathBuf>),
    AuthHeaders(std::collections::HashMap<String, String>),
    AuthCredentialCommand(String),
    ExecEnv(super::ExecEnvironment),
    ExecEnvAllowlist(Vec<String>),
    Network(super::Network),
    Budget(Budget),
    Bare(bool),
    PermissionsJudge(ModelRef),
    StatuslineSegments(Vec<StatusSegmentName>),
    Path(PathBuf),
}

impl ManagedConfigValue {
    /// Parse exactly one TOML value according to `key`'s existing schema type.
    pub fn parse(key: ManagedConfigKey, raw: &str) -> Result<Self, UserConfigError> {
        let value = raw.parse::<toml::Value>().map_err(|error| {
            UserConfigError::new(format!(
                "invalid TOML value for managed configuration key `{key}`: {error}"
            ))
        })?;
        match &key {
            ManagedConfigKey::ExecPaths => decode(&key, value).map(Self::ExecPaths),
            ManagedConfigKey::AuthHeaders(_) => {
                let mut auth = toml::map::Map::new();
                auth.insert("headers".into(), value);
                let auth: super::layer::AuthProviderLayer = decode(&key, toml::Value::Table(auth))?;
                Ok(Self::AuthHeaders(auth.headers))
            }
            ManagedConfigKey::AuthCredentialCommand(_) => {
                decode(&key, value).map(Self::AuthCredentialCommand)
            }
            ManagedConfigKey::Model => decode(&key, value).map(Self::Model),
            ManagedConfigKey::Thinking => decode(&key, value).map(Self::Thinking),
            ManagedConfigKey::Mode => decode(&key, value).map(Self::Mode),
            ManagedConfigKey::Sandbox => decode(&key, value).map(Self::Sandbox),
            ManagedConfigKey::ExecEnv => decode(&key, value).map(Self::ExecEnv),
            ManagedConfigKey::Roots => decode(&key, value).map(Self::Roots),
            ManagedConfigKey::Network => decode(&key, value).map(Self::Network),
            ManagedConfigKey::ExecEnvAllowlist => {
                let names: Vec<String> = decode(&key, value)?;
                crate::permit::EnvPolicy::allow_only(names.iter().map(String::as_str))
                    .map_err(|error| UserConfigError::new(error.to_string()))?;
                Ok(Self::ExecEnvAllowlist(names))
            }
            ManagedConfigKey::Budget => decode(&key, value).map(Self::Budget),
            ManagedConfigKey::Bare => decode(&key, value).map(Self::Bare),
            ManagedConfigKey::PermissionsJudge => decode(&key, value).map(Self::PermissionsJudge),
            ManagedConfigKey::StatuslineSegments => {
                let segments: Vec<StatusSegmentName> = decode(&key, value)?;
                validate_unique_status_segments(&segments).map_err(|error| {
                    UserConfigError::new(format!(
                        "invalid value for managed configuration key `{key}`: {error}"
                    ))
                })?;
                Ok(Self::StatuslineSegments(segments))
            }
            ManagedConfigKey::SessionDir | ManagedConfigKey::ArtifactDir => {
                decode(&key, value).map(Self::Path)
            }
        }
    }

    /// Render the canonical TOML value for persistence; callers must redact secret keys.
    pub fn to_toml(&self) -> String {
        let value = match self {
            Self::Model(model) | Self::PermissionsJudge(model) => {
                toml::Value::String(model.0.clone())
            }
            Self::Thinking(thinking) => toml::Value::String(thinking.as_str().to_owned()),
            Self::Mode(mode) => toml::Value::String(mode.as_str().to_owned()),
            Self::ExecEnv(env) => toml::Value::String(
                match env {
                    super::ExecEnvironment::Inherit => "inherit",
                    super::ExecEnvironment::Core => "core",
                    super::ExecEnvironment::Allowlist => "allowlist",
                }
                .into(),
            ),
            Self::ExecEnvAllowlist(names) => {
                toml::Value::Array(names.iter().cloned().map(toml::Value::String).collect())
            }
            Self::Network(network) => toml::Value::String(
                match network {
                    super::Network::On => "on",
                    super::Network::Off => "off",
                }
                .into(),
            ),
            Self::Sandbox(sandbox) => toml::Value::String(sandbox.to_string()),
            Self::Budget(budget) => toml::Value::String(canonical_budget(budget)),
            Self::Bare(bare) => toml::Value::Boolean(*bare),
            Self::StatuslineSegments(segments) => toml::Value::Array(
                segments
                    .iter()
                    .map(|segment| toml::Value::String(segment.as_str().to_owned()))
                    .collect(),
            ),
            Self::Roots(roots) => toml::Value::try_from(roots).expect("roots are TOML data"),
            Self::ExecPaths(paths) => toml::Value::try_from(paths).expect("paths are TOML data"),
            Self::AuthHeaders(headers) => {
                toml::Value::try_from(headers).expect("headers are TOML data")
            }
            Self::AuthCredentialCommand(command) => toml::Value::String(command.clone()),
            Self::Path(path) => toml::Value::String(path.to_string_lossy().into_owned()),
        };
        value.to_string()
    }
}

fn decode<T>(key: &ManagedConfigKey, value: toml::Value) -> Result<T, UserConfigError>
where
    T: DeserializeOwned,
{
    value.try_into().map_err(|error| {
        UserConfigError::new(format!(
            "invalid value for managed configuration key `{key}`: {error}"
        ))
    })
}

fn canonical_budget(budget: &Budget) -> String {
    match budget {
        Budget::Money(money) if money.currency.as_str() == "USD" => {
            format!("{}$", money.amount)
        }
        Budget::Money(money) => format!("{}{}", money.amount, money.currency),
        Budget::Tokens(tokens) if tokens.is_multiple_of(1_000_000) => {
            format!("{}m", tokens / 1_000_000)
        }
        Budget::Tokens(tokens) if tokens.is_multiple_of(1_000) => {
            format!("{}k", tokens / 1_000)
        }
        Budget::Tokens(tokens) => tokens.to_string(),
    }
}
