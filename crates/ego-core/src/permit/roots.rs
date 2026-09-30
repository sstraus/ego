//! Profile roots: extra canonical directories a profile opens to the run.
//!
//! The workspace stays the one boundary ego draws for itself. A profile may
//! widen it with named roots, each read-only or read-write, but only an
//! operator-owned layer can say so (`config::layer`) and only after
//! [`admit_roots`] has resolved every entry.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{CanonPath, PathResolver};

/// What a run may do under a root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootAccess {
    Read,
    ReadWrite,
}

/// One root as written in a profile: a path, before resolution.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootCfg {
    pub path: PathBuf,
    pub access: RootAccess,
}

/// One admitted root: canonical, and checked against the ceilings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRoot {
    path: CanonPath,
    access: RootAccess,
}

impl ProfileRoot {
    pub fn new(path: CanonPath, access: RootAccess) -> Self {
        Self { path, access }
    }

    pub fn path(&self) -> &CanonPath {
        &self.path
    }

    pub fn access(&self) -> RootAccess {
        self.access
    }
}

/// Why a profile's roots were refused at load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootsError {
    NotAbsolute {
        path: PathBuf,
    },
    Unresolvable {
        path: PathBuf,
        reason: String,
    },
    Overbroad {
        path: CanonPath,
        contains: &'static str,
    },
}

impl std::fmt::Display for RootsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RootsError::NotAbsolute { path } => write!(
                f,
                "profile root {} must be an absolute path or start with `~/`",
                path.display()
            ),
            RootsError::Unresolvable { path, reason } => {
                write!(
                    f,
                    "profile root {} cannot be resolved: {reason}",
                    path.display()
                )
            }
            RootsError::Overbroad { path, contains } => {
                write!(
                    f,
                    "profile root {path} is too broad: it contains {contains}"
                )
            }
        }
    }
}

impl std::error::Error for RootsError {}

/// Resolve and vet a profile's roots.
///
/// Ceilings are judged on the canonical path, so a symlink cannot smuggle a
/// broad directory past them. `home` and `/etc/ego` (the administrator's
/// ceiling directory) must stay outside every root: a root may not be, or
/// contain, either of them.
pub fn admit_roots(
    roots: &[RootCfg],
    home: &CanonPath,
    resolver: &dyn PathResolver,
) -> Result<Vec<ProfileRoot>, RootsError> {
    roots
        .iter()
        .map(|root| {
            let expanded = expand(&root.path, home)?;
            let path =
                resolver
                    .canonicalize(&expanded)
                    .map_err(|error| RootsError::Unresolvable {
                        path: root.path.clone(),
                        reason: error.to_string(),
                    })?;
            for (protected, contains) in [
                (home.as_path(), "the home directory"),
                (
                    Path::new(ADMIN_DIR),
                    "the administrator's ceilings in /etc/ego",
                ),
            ] {
                if protected.starts_with(path.as_path()) {
                    return Err(RootsError::Overbroad { path, contains });
                }
            }
            Ok(ProfileRoot::new(path, root.access))
        })
        .collect()
}

const ADMIN_DIR: &str = "/etc/ego";

fn expand(path: &Path, home: &CanonPath) -> Result<PathBuf, RootsError> {
    if let Ok(rest) = path.strip_prefix("~") {
        return Ok(home.as_path().join(rest));
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Err(RootsError::NotAbsolute {
            path: path.to_path_buf(),
        })
    }
}
