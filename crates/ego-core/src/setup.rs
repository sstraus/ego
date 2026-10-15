//! Closed setup diagnostics. This controller has no process, tool, or writer port.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Ruleset;
use crate::permit::{CanonPath, Classification, Op, PolicyEngine, PreparedCall, Resolution};

/// The only model-selectable probe operations. Unknown kinds or fields fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Probe {
    Action {
        argv: Vec<String>,
        cwd: PathBuf,
        reason: String,
    },
    Read {
        path: PathBuf,
        reason: String,
    },
    Metadata {
        path: PathBuf,
        reason: String,
    },
    Write {
        path: PathBuf,
        reason: String,
    },
    Tool {
        name: String,
        reason: String,
    },
    Git {
        path: PathBuf,
        reason: String,
    },
    Instructions {
        path: PathBuf,
        reason: String,
    },
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
    /// Read-only action preparation; never launches a process or grants authority.
    fn action(&self, _argv: &[String], _cwd: &std::path::Path) -> Result<PreparedCall, String> {
        Err("concrete action observation unavailable".into())
    }
    fn metadata(&self, path: &std::path::Path) -> Result<Observation, String>;
    fn write_metadata(&self, path: &std::path::Path) -> Result<Observation, String> {
        self.metadata(path)
    }
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
        if let Probe::Action { argv, cwd, reason } = probe {
            let mut finding = Finding {
                id,
                reason: crate::permit::terminal_safe(reason),
                requested: argv.first().map(PathBuf::from).unwrap_or_default(),
                canonical: None,
                status: Status::Unknown,
                evidence: String::new(),
                observed: false,
                truncated: false,
            };
            match self.reader.action(argv, cwd) {
                Ok(call) => {
                    if let [Op::Spawn { spec }] = call.ops() {
                        finding.canonical = self
                            .reader
                            .metadata(std::path::Path::new(&spec.argv()[0]))
                            .ok()
                            .map(|observed| observed.canonical.as_path().to_path_buf());
                    }
                    finding.status = match self.policy.classify(call) {
                        Classification::Settled(Ok(_)) => Status::Ok,
                        Classification::Settled(Err(_)) => Status::Blocked,
                        Classification::Ask(pending) => {
                            match futures::executor::block_on(self.policy.settle(pending)) {
                                Resolution::Settled(Ok(_)) => Status::Ok,
                                _ => Status::Blocked,
                            }
                        }
                    };
                    finding.evidence = "concrete argv policy-only; executable/cwd metadata observed; image identity, native effects and dependencies unproven; no execution".into();
                }
                Err(error) => finding.evidence = crate::permit::terminal_safe(&error),
            }
            return finding;
        }
        let (path, reason, content) = match probe {
            Probe::Read { path, reason }
            | Probe::Git { path, reason }
            | Probe::Instructions { path, reason } => (path.clone(), reason, true),
            Probe::Metadata { path, reason } | Probe::Write { path, reason } => {
                (path.clone(), reason, false)
            }
            Probe::Tool { name, reason } => (PathBuf::from(name), reason, false),
            Probe::Action { .. } => unreachable!(),
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
            Probe::Write { path, .. } => self.reader.write_metadata(path),
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
        if matches!(probe, Probe::Tool { .. }) {
            finding.status = Status::Unknown;
            finding.evidence = format!(
                "PATH tool present; identity {}; executable reach and complete action are not proven; no execution",
                observation.identity
            );
            return finding;
        }
        let intended = if matches!(probe, Probe::Write { .. }) {
            Op::Write {
                path: observation.canonical.clone(),
                change: crate::permit::Change::Whole(Vec::new()),
            }
        } else {
            Op::Read {
                path: observation.canonical.clone(),
            }
        };
        let read = PreparedCall::new(
            if matches!(probe, Probe::Write { .. }) {
                "write"
            } else {
                "read"
            },
            intended,
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
        if matches!(probe, Probe::Write { .. }) {
            finding.evidence = format!(
                "policy-only write classification; identity {}; no write performed; native floor/backend result not proven",
                observation.identity
            );
            if matches!(finding.status, Status::Ok) {
                finding.status = Status::Unknown;
            }
            return finding;
        }
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
            r#"{"kind":"execute","path":"x","reason":"task"}"#,
            r#"{"kind":"read","path":"x","reason":"task","apply":true}"#,
        ] {
            assert!(serde_json::from_str::<super::Probe>(value).is_err());
        }
    }
}
