//! Captured adaptive authority, resolved by the host before any child runs.

use super::CanonPath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptivePerimeter {
    pub home: CanonPath,
    pub writable: Vec<CanonPath>,
    pub credentials: Vec<CanonPath>,
    pub controls: Vec<CanonPath>,
}
