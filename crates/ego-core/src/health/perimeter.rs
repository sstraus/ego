//! The operator-visible projection of admitted run authority.

use crate::config::{Effective, Source};
use crate::permit::{ExecCaps, ExecObservation, ProfileRoot, RootAccess, RunConfinement};
use crate::store::evidence::EvidenceRef;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// A read-only projection of the authorities already selected for a run.
#[derive(Debug, Clone, Serialize)]
pub struct Perimeter {
    workspace: PathBuf,
    roots: Vec<PerimeterRoot>,
    sandbox: String,
    sandbox_source: String,
    backend: String,
    capabilities: CapabilityEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_evidence: Option<EvidenceRef>,
    network: &'static str,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum CapabilityEvidence {
    Measured(ExecCaps),
    NotChecked(&'static str),
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
        let mut warnings = root_warnings(confinement.workspace().as_path(), roots);
        if confinement.level() == crate::permit::Confinement::Off {
            warnings.push(
                "open-perimeter: sandbox is off; exec children have unrestricted filesystem access"
                    .into(),
            );
        }
        Self {
            warnings,
            workspace: confinement.workspace().as_path().to_path_buf(),
            roots: roots
                .iter()
                .map(|root| PerimeterRoot {
                    path: root.path().as_path().to_path_buf(),
                    access: root.access(),
                    source: origin(effective.roots_source),
                })
                .chain(
                    confinement
                        .adaptive_perimeter()
                        .into_iter()
                        .flat_map(|p| &p.writable)
                        .map(|path| PerimeterRoot {
                            path: path.as_path().to_path_buf(),
                            access: RootAccess::ReadWrite,
                            source: "derived".to_owned(),
                        }),
                )
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
            }
            .into(),
            capabilities: CapabilityEvidence::NotChecked("not_checked"),
            capabilities_reason: Some(
                "no completed backend measurement is available; inspection does not run probes",
            ),
            probe_evidence: None,
            network: if confinement.offline() {
                "offline"
            } else {
                "online"
            },
        }
    }

    /// Project the same policy's existing observation and committed probe evidence.
    ///
    /// An observation without a probe transcript (for example the null backend)
    /// is not a behavioral measurement. This operation executes no probes.
    pub fn with_execution_observation(
        mut self,
        observation: &ExecObservation,
        probe_evidence: Option<&EvidenceRef>,
    ) -> Self {
        self.backend = observation.backend().name().to_owned();
        self.capabilities = match probe_evidence {
            Some(_) => CapabilityEvidence::Measured(observation.capabilities()),
            None => CapabilityEvidence::NotChecked("not_checked"),
        };
        self.capabilities_reason = probe_evidence
            .is_none()
            .then_some("the selected backend ran no capability probes");
        self.probe_evidence = probe_evidence.cloned();
        self
    }

    /// Completed behavioral guarantees, including an empty measured set.
    pub fn measured_capabilities(&self) -> Option<ExecCaps> {
        match self.capabilities {
            CapabilityEvidence::Measured(capabilities) => Some(capabilities),
            CapabilityEvidence::NotChecked(_) => None,
        }
    }

    /// Include process-edge diagnostics without exposing mutable projection state.
    pub fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        for warning in warnings {
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        self
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
            format!(
                "  capabilities\t{}",
                self.measured_capabilities()
                    .map_or_else(|| "not_checked".into(), |caps| caps.to_string())
            ),
            format!("  network\t{}\tinvocation", self.network),
        ]);
        if let Some(reason) = self.capabilities_reason {
            lines.push(format!("  capabilities_reason\t{reason}"));
        }
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
