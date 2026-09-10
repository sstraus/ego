//! Measured confinement admission for one ACP connection.
//!
//! The controls seat owns the immutable per-session snapshot. This adapter
//! owns only its effectful initialization: one lease, the real process probe,
//! and the durable observation events that make the probe evidence reachable.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ego_acp::serve::{SessionConfinementAdmission, SessionConfinementFuture};
use ego_core::control::ControlError;
use ego_core::event::domain::EventKind;
use ego_core::event::id::{Producer, SessionId, TurnId};
use ego_core::permit::{CanonPath, Confinement, RunConfinement};
use ego_core::run::RunRoot;
use ego_core::sink::NullSink;
use ego_core::turn::{CancelToken, Journal};
use ego_tools::clock::SystemClock;
use ego_tools::exec::{ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts};
use ego_tools::http::SystemResolver;
use ego_tools::store::JsonlStore;
use futures::future::{BoxFuture, FutureExt as _, Shared};

use crate::acp_controls::AcpSessionControls;
use crate::confinement_diagnostics::Reporter;
use crate::output::Stderr;

const SESSION_EVIDENCE_CAPACITY: u64 = 64 * 1024 * 1024;

type InitializationFuture = Shared<BoxFuture<'static, Result<(), ControlError>>>;

enum Initialization {
    Pending {
        root: RunRoot,
        attempt: Arc<()>,
        future: InitializationFuture,
    },
    Ready {
        root: RunRoot,
    },
}

/// The CLI-owned adapter that measures and pins one session's sandbox offer.
pub struct CliSessionConfinementAdmission {
    store: Arc<JsonlStore>,
    controls: Arc<AcpSessionControls>,
    ceiling: Confinement,
    cancel: CancelToken,
    reporter: Arc<Reporter>,
    initializations: Arc<Mutex<HashMap<SessionId, Initialization>>>,
}

impl CliSessionConfinementAdmission {
    pub fn new(
        store: Arc<JsonlStore>,
        controls: Arc<AcpSessionControls>,
        ceiling: Confinement,
        cancel: CancelToken,
    ) -> Self {
        Self::with_reporter(
            store,
            controls,
            ceiling,
            cancel,
            Arc::new(Reporter::new(Arc::new(Stderr))),
        )
    }

    pub(crate) fn with_reporter(
        store: Arc<JsonlStore>,
        controls: Arc<AcpSessionControls>,
        ceiling: Confinement,
        cancel: CancelToken,
        reporter: Arc<Reporter>,
    ) -> Self {
        Self {
            store,
            controls,
            ceiling,
            cancel,
            reporter,
            initializations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn root_refusal(pinned: &RunRoot, requested: &RunRoot) -> ControlError {
        ControlError::Refused {
            control: "sandbox",
            why: format!(
                "session confinement is pinned to `{}` and cannot be reinitialized for `{}`",
                pinned.as_path().display(),
                requested.as_path().display()
            ),
        }
    }
}

impl SessionConfinementAdmission for CliSessionConfinementAdmission {
    fn initialize(&self, session: SessionId, root: RunRoot) -> SessionConfinementFuture {
        let mut initializations = match self.initializations.lock() {
            Ok(initializations) => initializations,
            Err(_) => {
                return Box::pin(async {
                    Err(ControlError::Internal(
                        "the ACP confinement initialization lock is poisoned".to_owned(),
                    ))
                });
            }
        };

        let (attempt, future) = match initializations.get(&session) {
            Some(Initialization::Ready { root: pinned }) => {
                if pinned != &root {
                    let refusal = Self::root_refusal(pinned, &root);
                    return Box::pin(async move { Err(refusal) });
                }
                return Box::pin(async { Ok(()) });
            }
            Some(Initialization::Pending {
                root: pinned,
                attempt,
                future,
            }) => {
                if pinned != &root {
                    let refusal = Self::root_refusal(pinned, &root);
                    return Box::pin(async move { Err(refusal) });
                }
                (Arc::clone(attempt), future.clone())
            }
            None => {
                let attempt = Arc::new(());
                let future = initialize_once(
                    Arc::clone(&self.store),
                    Arc::clone(&self.controls),
                    self.ceiling,
                    self.cancel.clone(),
                    Arc::clone(&self.reporter),
                    session,
                    root.clone(),
                )
                .boxed()
                .shared();
                initializations.insert(
                    session,
                    Initialization::Pending {
                        root: root.clone(),
                        attempt: Arc::clone(&attempt),
                        future: future.clone(),
                    },
                );
                (attempt, future)
            }
        };
        drop(initializations);

        let initializations = Arc::clone(&self.initializations);
        Box::pin(async move {
            let result = future.await;
            let mut states = initializations.lock().map_err(|_| {
                ControlError::Internal(
                    "the ACP confinement initialization lock is poisoned".to_owned(),
                )
            })?;
            let is_current = matches!(
                states.get(&session),
                Some(Initialization::Pending {
                    attempt: current,
                    ..
                }) if Arc::ptr_eq(current, &attempt)
            );
            if is_current {
                if result.is_ok() {
                    states.insert(session, Initialization::Ready { root });
                } else {
                    states.remove(&session);
                }
            }
            result
        })
    }

    fn observation(
        &self,
        session: SessionId,
        confinement: Confinement,
    ) -> Result<ego_core::permit::ExecObservation, ControlError> {
        self.controls.observation(session, confinement)
    }
}

async fn initialize_once(
    store: Arc<JsonlStore>,
    controls: Arc<AcpSessionControls>,
    ceiling: Confinement,
    cancel: CancelToken,
    reporter: Arc<Reporter>,
    session: SessionId,
    root: RunRoot,
) -> Result<(), ControlError> {
    let clock = SystemClock;
    let opened = crate::session::open(store.as_ref(), &session, &clock).map_err(|error| {
        ControlError::Internal(format!(
            "the ACP session could not be opened for confinement measurement: {error}"
        ))
    })?;
    let evidence = opened
        .writer
        .evidence_store(SESSION_EVIDENCE_CAPACITY)
        .map_err(|error| {
            ControlError::Internal(format!(
                "the ACP session evidence store could not be opened: {error}"
            ))
        })?;

    let mut measured = Vec::new();
    for confinement in Confinement::ALL
        .into_iter()
        .filter(|candidate| candidate.within(ceiling))
    {
        let policy = RunConfinement::new(confinement, false, CanonPath::new(root.as_path()))
            .map_err(|error| {
                ControlError::Internal(format!(
                    "the ACP confinement policy could not be constructed: {error}"
                ))
            })?;
        let config = ExecRunConfig::new(
            policy.clone(),
            evidence.clone(),
            cancel.clone(),
            ExecLimits::default(),
            HttpCallTimeout::default(),
            None,
            Arc::new(SystemResolver::new()),
        );
        let (observation, probe_evidence) =
            RunEffectPorts::observe(&config).await.map_err(|error| {
                ControlError::Internal(format!(
                    "the `{confinement}` ACP confinement could not be measured: {error}"
                ))
            })?;
        measured.push((confinement, policy, observation, probe_evidence));
    }

    let sink = NullSink;
    let mut journal = Journal::new(
        session,
        &opened.writer,
        &clock,
        &sink,
        Producer::Main,
        opened.state.clone(),
    );
    let turn = TurnId::new();
    let mut supported = Vec::new();
    let mut unsupported = Vec::new();
    let mut ceiling_capabilities = None;
    for (confinement, policy, observation, probe_evidence) in measured {
        journal
            .append(
                turn,
                None,
                None,
                EventKind::ExecutionObserved {
                    observation: observation.clone(),
                    probe_evidence: probe_evidence.clone(),
                },
            )
            .map_err(|error| {
                ControlError::Internal(format!(
                    "the `{confinement}` ACP confinement observation could not be recorded: {error}"
                ))
            })?;

        if confinement == ceiling {
            ceiling_capabilities = Some(observation.capabilities());
        }

        match policy.requirements().check(observation.capabilities()) {
            Ok(()) => supported.push((confinement, observation)),
            Err(missing) => unsupported.push((
                confinement,
                missing
                    .with_probe_evidence(probe_evidence)
                    .from_backend(observation.backend().clone()),
            )),
        }
    }

    let ceiling_capabilities = ceiling_capabilities.ok_or_else(|| {
        ControlError::Internal("the effective ACP confinement ceiling was not measured".to_owned())
    })?;
    controls.seed_confinement(session, supported, unsupported)?;
    reporter.report(Ok(ceiling_capabilities));
    drop(journal);
    drop(evidence);
    drop(opened);
    Ok(())
}
