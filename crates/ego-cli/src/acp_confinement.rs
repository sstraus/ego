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
    shape: crate::cli::SessionShape,
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
            shape: crate::cli::SessionShape::default(),
            cancel,
            reporter,
            initializations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn with_shape(mut self, shape: crate::cli::SessionShape) -> Self {
        self.shape = shape;
        self
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
    fn configure_profiles(
        &self,
        session: SessionId,
        root: &RunRoot,
        profile: Option<&str>,
        ceiling: Option<&str>,
    ) -> Result<serde_json::Value, ControlError> {
        let mut shape = self.shape.clone();
        if let Some(profile) = profile {
            shape.profile = Some(profile.to_owned());
            shape.profile_source = Some(ego_core::config::Source::Flags);
        }
        let profile = shape.profile.clone();
        let mut args = crate::run::protocol_args(None, shape);
        args.ceiling_profile = ceiling.map(str::to_owned);
        let resolve = || {
            let layers =
                crate::config::layers(root.as_path(), &args, ego_core::config::Trust::Untrusted)?;
            crate::config::resolve_session_profile(
                &layers,
                ego_core::config::Trust::Untrusted,
                profile
                    .as_deref()
                    .map(|name| (name, ego_core::config::Source::Flags)),
                ceiling,
                root.as_path(),
            )
        };
        let effective =
            resolve().map_err(|error: crate::cli::UsageError| ControlError::Refused {
                control: "profile",
                why: error.to_string(),
            })?;
        let selected = effective.sandbox.unwrap_or_default();
        let sandbox = if selected.within(self.ceiling) {
            selected
        } else {
            self.ceiling
        };
        let result = serde_json::json!({"profile": profile, "ceilingProfile": ceiling,
            "effective": {"mode": effective.mode.unwrap_or(ego_core::config::Mode::Default),
                "sandbox": sandbox, "roots": effective.roots, "execPaths": effective.exec_paths},
            "warnings": effective.profile_warnings});
        self.controls
            .seed_profile(session, effective, profile, ceiling.map(str::to_owned))?;
        Ok(result)
    }

    fn profile_selections(
        &self,
        session: SessionId,
        selections: ego_core::run::RunSelections,
    ) -> Result<ego_core::run::RunSelections, ControlError> {
        self.controls.profile_selections(session, selections)
    }

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

/// Measure every confinement a session under `ceiling` may be offered, into
/// the process-wide capability cache. Safe to run ahead of any session: the
/// cache holds measurement facts only, and single-flights concurrent callers.
pub(crate) async fn warm_process_cache(
    ceiling: Confinement,
    cancel: CancelToken,
    root: &std::path::Path,
) -> Result<(), ControlError> {
    for confinement in Confinement::ALL
        .into_iter()
        .filter(|candidate| candidate.within(ceiling))
    {
        let policy =
            RunConfinement::new(confinement, false, CanonPath::new(root)).map_err(|error| {
                ControlError::Internal(format!(
                    "the ACP confinement policy could not be constructed: {error}"
                ))
            })?;
        RunEffectPorts::diagnostic_capabilities(&policy, cancel.clone(), ExecLimits::default())
            .await
            .map_err(|error| {
                ControlError::Internal(format!(
                    "the `{confinement}` ACP confinement could not be measured: {error}"
                ))
            })?;
    }
    Ok(())
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
    // A background `session/new` probe must not hold the session writer lease:
    // the host may pause or close the newly returned session while measuring.
    // Warm the process cache first, then replay its transcript into this
    // session's ordinary evidence store under the short writer lease.
    warm_process_cache(ceiling, cancel.clone(), root.as_path()).await?;

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
