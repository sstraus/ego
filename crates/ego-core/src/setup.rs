//! Closed setup diagnostics. This controller has no process, tool, or writer port.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Ruleset;
use crate::permit::{CanonPath, Classification, Op, PolicyEngine, PreparedCall, Resolution};

/// The only model-selectable probe operations. Unknown kinds or fields fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Probe {
    Read { path: PathBuf, reason: String },
    Metadata { path: PathBuf, reason: String },
    Tool { name: String, reason: String },
    Git { path: PathBuf, reason: String },
    Instructions { path: PathBuf, reason: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Blocked,
    Unknown,
}

/// Bounded native evidence, not model-created observations.
#[derive(Debug, Clone)]
pub struct Observation {
    pub requested: PathBuf,
    pub canonical: CanonPath,
    pub regular: bool,
    pub bytes: Vec<u8>,
    pub truncated: bool,
    pub identity: String,
}

/// Read-only external effect seam. Implementations cannot be supplied by model text.
pub trait ProbeReader {
    fn metadata(&self, path: &std::path::Path) -> Result<Observation, String>;
    fn read(&self, canonical: &CanonPath, max_bytes: usize) -> Result<Observation, String>;
    fn tool(&self, name: &str) -> Result<Observation, String>;
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub id: usize,
    pub reason: String,
    pub requested: PathBuf,
    pub canonical: Option<PathBuf>,
    pub status: Status,
    pub evidence: String,
    pub observed: bool,
    pub truncated: bool,
}

/// Exact content-read scope chosen by the host, never inferred from a file.
pub struct DiagnosticScope {
    paths: Vec<CanonPath>,
}

impl DiagnosticScope {
    pub fn new(paths: Vec<CanonPath>) -> Self {
        Self { paths }
    }
    fn contains(&self, path: &CanonPath) -> bool {
        self.paths.contains(path)
    }
}

/// Probes are finite; an unsupported model request cannot reach general tools.
pub struct Controller<'a> {
    reader: &'a dyn ProbeReader,
    policy: &'a PolicyEngine,
    scope: &'a DiagnosticScope,
}

impl<'a> Controller<'a> {
    pub fn new(
        reader: &'a dyn ProbeReader,
        policy: &'a PolicyEngine,
        scope: &'a DiagnosticScope,
    ) -> Self {
        Self {
            reader,
            policy,
            scope,
        }
    }

    pub fn inspect(&self, probes: &[Probe]) -> Result<Vec<Finding>, String> {
        if probes.len() > 64 {
            return Err("setup probe limit exceeded".into());
        }
        Ok(probes
            .iter()
            .enumerate()
            .map(|(id, probe)| self.inspect_one(id, probe))
            .collect())
    }

    fn inspect_one(&self, id: usize, probe: &Probe) -> Finding {
        let (path, reason, content) = match probe {
            Probe::Read { path, reason }
            | Probe::Git { path, reason }
            | Probe::Instructions { path, reason } => (path.clone(), reason, true),
            Probe::Metadata { path, reason } => (path.clone(), reason, false),
            Probe::Tool { name, reason } => (PathBuf::from(name), reason, false),
        };
        let mut finding = Finding {
            id,
            reason: crate::permit::terminal_safe(reason),
            requested: path.clone(),
            canonical: None,
            status: Status::Unknown,
            evidence: String::new(),
            observed: false,
            truncated: false,
        };
        if path.is_absolute() && Ruleset::is_credential_path(&CanonPath::new(&path)) {
            finding.status = Status::Blocked;
            finding.evidence = "credential diagnostic access is forbidden".into();
            return finding;
        }
        let observed = match probe {
            Probe::Tool { name, .. } => self.reader.tool(name),
            _ => self.reader.metadata(&path),
        };
        let observation = match observed {
            Ok(observation) => observation,
            Err(error) => {
                if error == "credential diagnostic access is forbidden" {
                    finding.status = Status::Blocked;
                }
                finding.evidence = crate::permit::terminal_safe(&error);
                return finding;
            }
        };
        finding.canonical = Some(observation.canonical.as_path().to_path_buf());
        // Diagnostic authority never lifts credential floors, including aliases.
        if Ruleset::is_credential_path(&observation.canonical) {
            finding.status = Status::Blocked;
            finding.evidence = "credential diagnostic access is forbidden".into();
            return finding;
        }
        let read = PreparedCall::new(
            "read",
            Op::Read {
                path: observation.canonical.clone(),
            },
        );
        let (status, policy_reason) = match self.policy.classify(read) {
            Classification::Settled(Ok(_)) => (Status::Ok, "target read is admitted"),
            Classification::Settled(Err(_)) => (Status::Blocked, "target read is denied"),
            Classification::Ask(pending) => {
                match futures::executor::block_on(self.policy.settle(pending)) {
                    Resolution::Settled(Ok(_)) => (Status::Ok, "target read is admitted by mode"),
                    _ => (Status::Blocked, "target read needs approval"),
                }
            }
        };
        finding.status = status;
        finding.evidence = format!("{policy_reason}; identity {}", observation.identity);
        if !content {
            return finding;
        }
        if !observation.regular {
            finding.status = Status::Unknown;
            finding.evidence = "content probe requires a regular file".into();
            return finding;
        }
        if !matches!(finding.status, Status::Ok) && !self.scope.contains(&observation.canonical) {
            finding
                .evidence
                .push_str("; exact diagnostic content scope not selected");
            return finding;
        }
        match self.reader.read(&observation.canonical, 64 * 1024) {
            Ok(fresh) if fresh.identity == observation.identity => {
                finding.observed = true;
                finding.truncated = fresh.truncated;
                finding
                    .evidence
                    .push_str(&format!("; observed {} bytes", fresh.bytes.len()));
            }
            Ok(_) => {
                finding.status = Status::Unknown;
                finding.evidence = "path identity changed during probe".into();
            }
            Err(error) => {
                finding.status = Status::Unknown;
                finding.evidence = crate::permit::terminal_safe(&error);
            }
        }
        finding
    }
}

#[cfg(test)]
mod tests {
    // Catches: model/tool text smuggles a write or extra authority field into probes.
    #[test]
    fn unsupported_setup_probe_cannot_enter_controller() {
        for value in [
            r#"{"kind":"write","path":"x","reason":"task"}"#,
            r#"{"kind":"read","path":"x","reason":"task","apply":true}"#,
        ] {
            assert!(serde_json::from_str::<super::Probe>(value).is_err());
        }
    }
}
