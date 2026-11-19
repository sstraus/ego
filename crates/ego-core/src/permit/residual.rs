//! The session desk between a confined executor that saw a refused write and
//! the turn loop that owns the shared judge.
//!
//! The executor has the floors and the canonical paths; the loop has the
//! call, the task and the judge. Neither can decide alone, so a refusal
//! travels here as one bounded request and comes back as one decision. The
//! desk keeps the session's judge grants: they outlive the per-turn executor
//! and die with the session. A judge grant is never a human grant and never
//! persists.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use serde::Serialize;

use super::{CanonPath, SpawnSpec};

/// Judge decisions one command may raise.
pub const MAX_DECISIONS_PER_CALL: usize = 3;
/// Judge decisions one session may raise. Every grant is a decision, so
/// this also bounds the session's automatic grants to 32.
pub const MAX_DECISIONS_PER_SESSION: usize = 32;
/// Hard bound on one decision, including provider queueing.
pub const DECISION_DEADLINE: Duration = Duration::from_secs(5);

/// Bounded, untrusted facts about one refused write candidate. Names and
/// stderr come from a process the model chose; they are evidence only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResidualWriteEvidence {
    /// The path exactly as the child printed it.
    pub requested: String,
    /// The exact canonical path a grant would open.
    pub candidate: CanonPath,
    pub exists: bool,
    pub kind: ResidualPathKind,
    /// Whether the candidate (or its existing parent) belongs to the user
    /// who owns the workspace; `None` when ownership was not observable.
    pub owned_by_workspace_user: Option<bool>,
    pub listing: ResidualListing,
    /// The refusal line, at most 2 KiB. Never proof of an attempted write.
    pub refusal: String,
    /// Writable roots already in force: workspace, derived and judge grants.
    pub writable: Vec<CanonPath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidualPathKind {
    Dotfile,
    SystemDirectory,
    OtherRepository,
    Other,
}

/// Names, sizes and types of at most 32 entries, never contents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResidualListing {
    pub complete: bool,
    pub entries: Vec<ResidualEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResidualEntry {
    pub name: String,
    pub bytes: u64,
    pub kind: &'static str,
}

/// The scope the judge recommended. Only `Session` is ever applied here:
/// `Always` needs a human to write the persistent ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidualScope {
    Session,
    Always,
}

/// What came back for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResidualDecision {
    /// Journaled and published: the path is writable for the session.
    Granted {
        path: CanonPath,
        recommended: ResidualScope,
        reason: String,
    },
    /// The judge answered none, or its write failed the recheck.
    Refused { reason: String },
    /// No usable opinion: missing judge, timeout, invalid verdict, stale
    /// binding, exhausted limits or a closed desk.
    Unavailable { reason: String },
}

/// The journaled kind of a [`ResidualDecision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidualOutcome {
    Granted,
    Refused,
    Unavailable,
}

impl ResidualDecision {
    pub fn outcome(&self) -> ResidualOutcome {
        match self {
            Self::Granted { .. } => ResidualOutcome::Granted,
            Self::Refused { .. } => ResidualOutcome::Refused,
            Self::Unavailable { .. } => ResidualOutcome::Unavailable,
        }
    }

    pub fn reason(&self) -> &str {
        match self {
            Self::Granted { reason, .. }
            | Self::Refused { reason }
            | Self::Unavailable { reason } => reason,
        }
    }
}

/// The recheck the executor owns: floors, breadth and identity of a path the
/// judge returned. `Err` names why the path cannot be opened.
pub type Admissible = Box<dyn Fn(&CanonPath) -> Result<(), String> + Send + Sync>;

/// One refusal waiting for the loop.
pub struct ResidualWriteRequest {
    pub spec: SpawnSpec,
    pub evidence: ResidualWriteEvidence,
    pub admissible: Admissible,
    reply: oneshot::Sender<ResidualDecision>,
}

#[derive(Default)]
struct Ledger {
    grants: Vec<CanonPath>,
    decisions: usize,
}

/// One session's residual-write authority.
pub struct ResidualWriteDesk {
    ledger: Mutex<Ledger>,
    sender: mpsc::UnboundedSender<ResidualWriteRequest>,
    receiver: futures::lock::Mutex<mpsc::UnboundedReceiver<ResidualWriteRequest>>,
}

impl std::fmt::Debug for ResidualWriteDesk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResidualWriteDesk")
            .field("grants", &self.grants())
            .finish_non_exhaustive()
    }
}

impl ResidualWriteDesk {
    pub fn new() -> Arc<Self> {
        let (sender, receiver) = mpsc::unbounded();
        Arc::new(Self {
            ledger: Mutex::default(),
            sender,
            receiver: futures::lock::Mutex::new(receiver),
        })
    }

    fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every path the judge opened for writing this session. Each one also
    /// stays non-executable for the rest of the session.
    pub fn grants(&self) -> Vec<CanonPath> {
        self.ledger().grants.clone()
    }

    /// Executor side: put one refusal to the loop and wait for its answer.
    /// The caller bounds the wait; a request nobody serves never grants.
    pub async fn decide(
        &self,
        spec: SpawnSpec,
        evidence: ResidualWriteEvidence,
        admissible: Admissible,
    ) -> ResidualDecision {
        {
            let mut ledger = self.ledger();
            if ledger.decisions >= MAX_DECISIONS_PER_SESSION {
                return unavailable("the session used all residual judge decisions");
            }
            ledger.decisions += 1;
        }
        let (reply, answer) = oneshot::channel();
        let request = ResidualWriteRequest {
            spec,
            evidence,
            admissible,
            reply,
        };
        if self.sender.unbounded_send(request).is_err() {
            return unavailable("the residual judge desk is closed");
        }
        answer
            .await
            .unwrap_or_else(|_| unavailable("no judge answered the residual write"))
    }

    /// Loop side: publish a journaled decision. A grant enters the session
    /// ledger here, before the executor hears of it, so an executor that
    /// stopped waiting cannot leave a journaled grant unapplied.
    pub fn answer(&self, request: ResidualWriteRequest, decision: ResidualDecision) {
        if let ResidualDecision::Granted { path, .. } = &decision {
            let mut ledger = self.ledger();
            if !ledger.grants.contains(path) {
                ledger.grants.push(path.clone());
            }
        }
        let _ = request.reply.send(decision);
    }

    /// Loop side: the next refusal waiting for an opinion.
    pub async fn next(&self) -> Option<ResidualWriteRequest> {
        self.receiver.lock().await.next().await
    }
}

fn unavailable(reason: &str) -> ResidualDecision {
    ResidualDecision::Unavailable {
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
#[path = "residual_tests.rs"]
mod tests;
