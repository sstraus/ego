//! Transient setup entry. Never constructs an ordinary run or its effects.

use std::path::PathBuf;

use ego_core::config::{Source, Trust, resolve_selected};
use ego_core::permit::{PathResolver, PolicyEngine};
use ego_core::run::RunTermination;
use ego_core::setup::{Controller, DiagnosticScope, Probe};
use ego_tools::fs::OsFs;
use ego_tools::setup::NativeProbeReader;

use crate::cli::{SetupArgs, UsageError};
use crate::config;
use crate::output::Stdout;

/// Both host entry points render this same transient inventory.
pub fn observed(workspace: PathBuf, args: &SetupArgs) -> Result<String, UsageError> {
    let workspace = OsFs
        .canonicalize(&workspace)
        .map_err(|error| UsageError(format!("setup workspace unavailable: {error}")))?;
    let home = config::ego_home();
    let layers = config::base_layers_in(workspace.as_path(), Trust::Untrusted, home.as_deref())?;
    let env_profile = std::env::var("EGO_PROFILE").ok();
    let selected = env_profile.as_deref().map(|name| (name, Source::Env));
    let mut effective = resolve_selected(&layers, Trust::Untrusted, selected)
        .map_err(|error| UsageError(error.to_string()))?;
    if let Some(home) = std::env::var_os("HOME") {
        crate::run::profile_roots::defaults(&mut effective, std::path::Path::new(&home));
    }
    let roots = crate::run::profile_roots::admit(&effective, &workspace).map_err(UsageError)?;
    let policy = PolicyEngine::new(
        workspace.clone(),
        effective.mode.unwrap_or(ego_core::config::Mode::Default),
        effective.permissions,
    )
    .with_profile_roots(roots);
    let reader = NativeProbeReader::capture();
    let scope = DiagnosticScope::new(Vec::new());
    let probes = ["AGENTS.md", "CLAUDE.md", ".git"].map(|name| Probe::Metadata {
        path: workspace.as_path().join(name),
        reason: "default setup inventory".into(),
    });
    let findings = Controller::new(&reader, &policy, &scope)
        .inspect(&probes)
        .map_err(UsageError)?;
    let target = args.profile.as_deref().unwrap_or("base user config");
    let target = ego_core::permit::terminal_safe(target);
    let mut rendered =
        format!("Setup target: {target}\nReport only; captured session authority is unchanged.\n");
    for item in findings {
        rendered.push_str(&format!(
            "{} {:?}: {} — {}\n",
            item.id,
            item.status,
            ego_core::permit::terminal_safe(&item.requested.display().to_string()),
            item.evidence
        ));
    }
    Ok(rendered)
}

pub fn execute(args: &SetupArgs, out: &Stdout) -> RunTermination {
    let workspace = args
        .directory
        .clone()
        .or_else(|| std::env::current_dir().ok());
    let Some(workspace) = workspace else {
        return RunTermination::Failed {
            message: "setup workspace unavailable".into(),
        };
    };
    match observed(workspace, args) {
        Ok(report) => {
            out.line(&report);
            RunTermination::Done
        }
        Err(error) => RunTermination::Failed {
            message: error.to_string(),
        },
    }
}
