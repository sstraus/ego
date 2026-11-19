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

/// Built-in filesystem read authority derived from the admitted sandbox.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadPerimeter {
    broad: bool,
    credential_floor: bool,
    exclusions: Vec<CanonPath>,
}

impl ReadPerimeter {
    pub fn from_confinement(confinement: &super::RunConfinement) -> Self {
        Self {
            broad: matches!(
                confinement.level(),
                super::Confinement::Off | super::Confinement::Adaptive
            ),
            credential_floor: matches!(
                confinement.level(),
                super::Confinement::Off | super::Confinement::Adaptive
            ),
            exclusions: if confinement.level() == super::Confinement::Adaptive {
                confinement
                    .adaptive_perimeter()
                    .map(|perimeter| perimeter.credentials.clone())
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
        }
    }

    pub(crate) fn broad(&self) -> bool {
        self.broad
    }

    pub(crate) fn refuses(&self, path: &CanonPath) -> bool {
        (self.credential_floor && crate::config::Ruleset::is_credential_path(path))
            || self.exclusions.iter().any(|base| path.is_or_under(base))
    }

    pub(crate) fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
