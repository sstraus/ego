//! Configuration errors with source attribution.

use super::Source;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A layer contained a key that isn't part of the schema.
    UnknownKey { key: String, source: Source },
    /// The layer failed to parse for any other reason (syntax error, wrong
    /// value type, ...); `message` is `toml`'s own diagnostic.
    Invalid { source: Source, message: String },
    /// `active_profile` named a profile that no eligible layer defines.
    UnknownProfile { name: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::UnknownKey { key, source } => {
                write!(f, "unknown config key `{key}` in {source} layer")
            }
            ConfigError::Invalid { source, message } => {
                write!(f, "invalid config in {source} layer: {message}")
            }
            ConfigError::UnknownProfile { name } => {
                write!(
                    f,
                    "profile '{name}' is not defined in the configuration used by this session"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}
