//! Validated probabilities and credential references.
use serde::{Deserialize, Serialize};

/// Finite probability validated when deserialized.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Probability(pub(super) f64);
impl Eq for Probability {}
impl Probability {
    pub fn get(self) -> f64 {
        self.0
    }
}
impl<'de> Deserialize<'de> for Probability {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let p = f64::deserialize(d)?;
        if !p.is_finite() || !(0.0..=1.0).contains(&p) {
            return Err(serde::de::Error::custom(
                "jev threshold must be within 0..1",
            ));
        }
        Ok(Self(p))
    }
}
/// Authentication references; API key material cannot be represented here.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum JevAuth {
    ApiKey { env: String },
    Header { name: String, value: String },
}
impl std::fmt::Debug for JevAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey { env } => f.debug_struct("ApiKey").field("env", env).finish(),
            Self::Header { name, .. } => f
                .debug_struct("Header")
                .field("name", name)
                .finish_non_exhaustive(),
        }
    }
}
impl Default for JevAuth {
    fn default() -> Self {
        Self::ApiKey {
            env: "OPENROUTER_API_KEY".into(),
        }
    }
}
