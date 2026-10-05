//! The operator-visible projection of admitted run authority.

use crate::config::{Effective, Source};
use crate::permit::{ProfileRoot, RootAccess, RunConfinement};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// A read-only projection of the authorities already selected for a run.
#[derive(Debug, Clone, Serialize)]
pub struct Perimeter {
    workspace: PathBuf,
    roots: Vec<PerimeterRoot>,
    sandbox: String,
    sandbox_source: String,
    backend: &'static str,
    capabilities: &'static str,
    network: &'static str,
    warnings: Vec<String>,
}

/// One canonical root admitted by the profile's permission policy.
#[derive(Debug, Clone, Serialize)]
pub struct PerimeterRoot {
    path: PathBuf,
    access: RootAccess,
    source: String,
}

impl Perimeter {
    /// Project the session's existing confinement and already admitted roots.
    pub fn session(
        confinement: &RunConfinement,
        effective: &Effective,
        roots: &[ProfileRoot],
    ) -> Self {
        Self {
            warnings: root_warnings(confinement.workspace().as_path(), roots),
            workspace: confinement.workspace().as_path().to_path_buf(),
            roots: roots
                .iter()
                .map(|root| PerimeterRoot {
                    path: root.path().as_path().to_path_buf(),
                    access: root.access(),
                    source: origin(effective.roots_source),
                })
                .collect(),
            sandbox: confinement.level().to_string(),
            sandbox_source: origin(effective.sandbox_source),
            backend: if confinement.level() == crate::permit::Confinement::Off
                && !(cfg!(target_os = "macos") && confinement.offline())
            {
                "none"
            } else if cfg!(target_os = "macos") {
                "seatbelt"
            } else if cfg!(target_os = "linux") {
                "landlock"
            } else {
                "unsupported"
            },
            capabilities: "not_checked",
            network: if confinement.offline() {
                "offline"
            } else {
                "online"
            },
        }
    }

    /// The same terminal block on all three surfaces.
    pub fn text(&self) -> String {
        let mut lines = vec![
            "perimeter:".to_owned(),
            format!("  workspace\t{}\tcwd", display_path(&self.workspace)),
        ];
        for root in &self.roots {
            lines.push(format!(
                "  root\t{}\t{}\t{}",
                display_path(&root.path),
                match root.access {
                    RootAccess::Read => "read",
                    RootAccess::ReadWrite => "read-write",
                },
                root.source
            ));
        }
        if self.roots.is_empty() {
            lines.push("  roots\tnone\tdefault".into());
        }
        lines.extend([
            format!("  sandbox\t{}\t{}", self.sandbox, self.sandbox_source),
            format!("  backend\t{}", self.backend),
            format!("  capabilities\t{}", self.capabilities),
            format!("  network\t{}\tinvocation", self.network),
        ]);
        lines.extend(
            self.warnings
                .iter()
                .map(|warning| format!("  warning\t{warning}")),
        );
        lines.join("\n")
    }
}

fn origin(source: Option<Source>) -> String {
    source.map_or_else(|| "default".into(), |source| source.to_string())
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .flat_map(|character| character.escape_debug())
        .collect()
}

fn root_warnings(workspace: &Path, roots: &[ProfileRoot]) -> Vec<String> {
    let mut warnings = Vec::new();
    for root in roots {
        let path = root.path().as_path();
        if path.starts_with(workspace) || workspace.starts_with(path) {
            warnings.push(format!(
                "root {} overlaps the workspace",
                display_path(path)
            ));
        }
        if root.access() == RootAccess::Read
            && roots.iter().any(|other| {
                other.access() == RootAccess::ReadWrite && path.starts_with(other.path().as_path())
            })
        {
            warnings.push(format!(
                "read root {} is covered by a read-write root",
                display_path(path)
            ));
        }
    }
    warnings
}
