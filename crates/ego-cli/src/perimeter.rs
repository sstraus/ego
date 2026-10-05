//! The effective perimeter, shared by stored-config inspection and session diagnostics.

use std::path::Path;

use ego_core::config::{Source, Trust, resolve_selected};
use ego_core::permit::{PathResolver, RunConfinement};
use ego_tools::fs::OsFs;

use crate::{cli::UsageError, config};

pub use ego_core::health::perimeter::Perimeter;

/// Resolve the read-only headless view without trusting ambient project files.
///
/// No backend capability probe, provider request, or configuration write occurs.
pub fn observe(
    workspace: &Path,
    home: Option<&Path>,
    profile: Option<&str>,
) -> Result<Perimeter, UsageError> {
    let workspace = OsFs
        .canonicalize(workspace)
        .map_err(|_| UsageError("the perimeter workspace cannot be resolved".into()))?;
    let layers = config::base_layers_in(workspace.as_path(), Trust::Untrusted, home)?;
    let env_profile = std::env::var("EGO_PROFILE").ok();
    let selected = profile
        .map(|name| (name, Source::Flags))
        .or_else(|| env_profile.as_deref().map(|name| (name, Source::Env)));
    let effective = resolve_selected(&layers, Trust::Untrusted, selected)
        .map_err(|error| UsageError(error.to_string()))?;
    let roots = crate::run::profile_roots::admit(&effective).map_err(|error| {
        UsageError(format!(
            "warning: configured perimeter roots could not be admitted: {error}"
        ))
    })?;
    let confinement = RunConfinement::new(effective.sandbox.unwrap_or_default(), false, workspace)
        .map_err(|error| UsageError(error.to_string()))?;
    Ok(Perimeter::session(&confinement, &effective, &roots))
}
