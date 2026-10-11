//! Captured adaptive authority, resolved by the host before any child runs.

use super::CanonPath;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdaptivePerimeter {
    pub home: CanonPath,
    pub writable: Vec<CanonPath>,
    pub credentials: Vec<CanonPath>,
    pub controls: Vec<CanonPath>,
}
