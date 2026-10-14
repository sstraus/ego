//! The ACP connection's operator seat for model, effort, mode and sandbox.
//!
//! `2026-09-03-acp-host-control-is-negotiated-per-session.md` makes the
//! connected client the person in an ACP-owned turn. This is the other half
//! of that: the picker that person is offered, and what a pick actually does.
//!
//! Model, effort and mode are answered from the same two sources the terminal
//! picker uses — the discovered model catalogue and the workspace's resolved
//! configuration. Sandbox choices come from the process observations the ACP
//! initializer pins before this seat is exposed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ego_ai::catalogue::{ModelAvailability, ModelCatalogueSnapshot};
use ego_core::config::{Effective, Mode, ModelRef};
use ego_core::control::{
    ControlError, ControlSelect, SessionControlChange, SessionControlState, SessionControls,
};
use ego_core::event::id::SessionId;
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::{Confinement, ExecObservation, MissingExecCaps};

use crate::model_picker::reasoning_levels;

/// Every mode an ACP host may select.
///
/// The host is the human seat and shows each mode before the operator selects
/// it. A selection changes only this session's subsequent generations.
const HOST_MODES: [Mode; 5] = [
    Mode::Plan,
    Mode::Default,
    Mode::Edits,
    Mode::Auto,
    Mode::Yolo,
];

/// What one ACP session's operator has picked, if anything.
#[derive(Debug, Clone, Default)]
struct Chosen {
    model: Option<ModelRef>,
    effort: Option<ThinkingLevel>,
    mode: Option<Mode>,
    sandbox: Option<Confinement>,
}

/// The measured confinement facts pinned before this session's picker exists.
#[derive(Debug, Clone)]
struct ConfinementMeasurements {
    supported: Vec<(Confinement, ExecObservation)>,
    unsupported: Vec<(Confinement, MissingExecCaps)>,
}

impl ConfinementMeasurements {
    fn observation(&self, confinement: Confinement) -> Option<&ExecObservation> {
        self.supported
            .iter()
            .find_map(|(candidate, observation)| (*candidate == confinement).then_some(observation))
    }

    fn refusal(&self, confinement: Confinement) -> Option<&MissingExecCaps> {
        self.unsupported
            .iter()
            .find_map(|(candidate, refusal)| (*candidate == confinement).then_some(refusal))
    }
}

#[derive(Debug, Clone)]
struct ProfileSeat {
    effective: Effective,
    profile: Option<String>,
    ceiling_profile: Option<String>,
    sandbox_ceiling: Confinement,
}

/// Everything one connection knows and may change for one session.
#[derive(Debug, Clone, Default)]
struct SessionSeat {
    read_only_dir: Option<std::path::PathBuf>,
    chosen: Chosen,
    confinement: Option<ConfinementMeasurements>,
    profile: Option<ProfileSeat>,
    prompt: Option<ego_core::config::SessionPromptOptions>,
}

/// One ACP connection's generation controls.
pub struct AcpSessionControls {
    store: Option<Arc<ego_tools::store::JsonlStore>>,
    model: Option<ModelRef>,
    effort: Option<ThinkingLevel>,
    mode: Mode,
    max_mode: Option<Mode>,
    judge: bool,
    /// The weakest confinement this connection may run at, and the value a
    /// session starts on.
    ///
    /// A ceiling rather than a default:
    /// `ARCHITECTURE.md`.
    /// The operator who spawned the process set it, and no message on this
    /// connection can move it.
    sandbox_ceiling: Confinement,
    catalogue: Option<ModelCatalogueSnapshot>,
    sessions: Mutex<HashMap<SessionId, SessionSeat>>,
}

impl std::fmt::Debug for AcpSessionControls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpSessionControls")
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("mode", &self.mode)
            .field("sandbox_ceiling", &self.sandbox_ceiling)
            .field("sessions", &self.sessions)
            .finish_non_exhaustive()
    }
}

impl AcpSessionControls {
    /// Seed the seat from the workspace configuration this connection serves.
    ///
    /// The catalogue is optional because discovery is an egress that can
    /// fail: a connection that could not enumerate models still offers mode,
    /// and offers only the configured model when one exists rather than a
    /// list it cannot vouch for.
    /// `ceiling` is the operator's, not the configuration's. It comes from
    /// `ego acp --sandbox`, and where that was not given it is the resolved
    /// `sandbox` — which for a headless untrusted resolution means the user
    /// layer, since Project and Local were discarded before this was read.
    pub fn new(
        effective: &Effective,
        ceiling: Confinement,
        catalogue: Option<ModelCatalogueSnapshot>,
    ) -> Self {
        let mut controls = Self::seeded(
            effective.model.clone(),
            effective.thinking,
            effective.mode.unwrap_or(Mode::Default),
            effective.judge.is_some(),
            ceiling,
            catalogue,
        );
        controls.max_mode = effective.max_mode;
        controls
    }

    /// The resolved values and whether a judge exists, without the rest of
    /// the configuration. This is the whole of what the seat needs, and
    /// saying so is what lets it be tested without building a workspace.
    pub fn seeded(
        model: Option<ModelRef>,
        effort: Option<ThinkingLevel>,
        mode: Mode,
        judge: bool,
        sandbox_ceiling: Confinement,
        catalogue: Option<ModelCatalogueSnapshot>,
    ) -> Self {
        Self {
            store: None,
            model,
            effort,
            mode,
            max_mode: None,
            judge,
            sandbox_ceiling,
            catalogue,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Keep explicit model choices in the session journal across ACP connections.
    pub(crate) fn with_store(mut self, store: Arc<ego_tools::store::JsonlStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Pin profile policy before measurement or any prompt can start.
    pub(crate) fn seed_profile(
        &self,
        session: SessionId,
        effective: Effective,
        profile: Option<String>,
        ceiling_profile: Option<String>,
    ) -> Result<(), ControlError> {
        let mut sessions = self.lock_sessions()?;
        let seat = sessions.entry(session).or_default();
        if let Some(pinned) = &seat.profile {
            return if pinned.profile == profile
                && pinned.ceiling_profile == ceiling_profile
                && pinned.effective == effective
            {
                Ok(())
            } else {
                Err(ControlError::Refused {
                    control: "profile",
                    why: "session profile policy changed after admission; start a new session"
                        .into(),
                })
            };
        }
        if seat.confinement.is_some() || seat.profile.is_some() {
            return Err(ControlError::Refused {
                control: "profile",
                why: "session policy is already pinned".into(),
            });
        }
        let selected = effective.sandbox.unwrap_or_default();
        let sandbox_ceiling = if selected.within(self.sandbox_ceiling) {
            selected
        } else {
            self.sandbox_ceiling
        };
        seat.profile = Some(ProfileSeat {
            effective,
            profile,
            ceiling_profile,
            sandbox_ceiling,
        });
        Ok(())
    }

    pub(crate) fn seed_read_only_dir(
        &self,
        session: SessionId,
        path: std::path::PathBuf,
    ) -> Result<(), ControlError> {
        let mut sessions = self.lock_sessions()?;
        let seat = sessions.entry(session).or_default();
        if seat
            .read_only_dir
            .as_ref()
            .is_some_and(|pinned| pinned != &path)
        {
            return Err(ControlError::Refused {
                control: "additionalDirectories",
                why: "the session read-only directory is already pinned".into(),
            });
        }
        seat.read_only_dir = Some(path);
        Ok(())
    }

    pub(crate) fn seed_prompt(
        &self,
        session: SessionId,
        options: ego_core::config::SessionPromptOptions,
    ) -> Result<(), ControlError> {
        self.lock_sessions()?.entry(session).or_default().prompt = Some(options);
        Ok(())
    }

    pub(crate) fn profile_selections(
        &self,
        session: SessionId,
        mut selections: ego_core::run::RunSelections,
    ) -> Result<ego_core::run::RunSelections, ControlError> {
        let sessions = self.lock_sessions()?;
        if let Some(path) = sessions
            .get(&session)
            .and_then(|seat| seat.read_only_dir.as_ref())
        {
            selections = selections.with_read_only_dir(path.clone());
        }
        if let Some(prompt) = sessions.get(&session).and_then(|seat| seat.prompt.as_ref()) {
            selections = selections.with_prompt_options(prompt.clone());
        }
        if let Some(profile) = sessions
            .get(&session)
            .and_then(|seat| seat.profile.as_ref())
        {
            if let Some(name) = &profile.profile {
                selections = selections.with_profile_name(name);
            }
            if let Some(name) = &profile.ceiling_profile {
                selections = selections.with_ceiling_profile(name);
            }
        }
        Ok(selections)
    }

    fn session_ceiling(&self, seat: &SessionSeat) -> Confinement {
        seat.profile
            .as_ref()
            .map_or(self.sandbox_ceiling, |profile| profile.sandbox_ceiling)
    }

    fn session_max_mode(&self, seat: &SessionSeat) -> Option<Mode> {
        seat.profile
            .as_ref()
            .map_or(self.max_mode, |profile| profile.effective.max_mode)
    }

    fn lock_sessions(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<SessionId, SessionSeat>>, ControlError> {
        self.sessions
            .lock()
            .map_err(|_| ControlError::Internal("the ACP control lock is poisoned".to_owned()))
    }

    fn uninitialized() -> ControlError {
        ControlError::Internal(
            "the ACP sandbox options were read before confinement initialization completed"
                .to_owned(),
        )
    }

    /// Install the complete measured offer exactly once.
    ///
    /// Validation happens before the session map is touched, so duplicate or
    /// contradictory rows cannot leave a partial offer behind. The adapter
    /// calls this only after every corresponding observation event is durable.
    pub fn seed_confinement(
        &self,
        session: SessionId,
        supported: Vec<(Confinement, ExecObservation)>,
        unsupported: Vec<(Confinement, MissingExecCaps)>,
    ) -> Result<(), ControlError> {
        let mut seen = Vec::new();
        for confinement in supported
            .iter()
            .map(|(confinement, _)| *confinement)
            .chain(unsupported.iter().map(|(confinement, _)| *confinement))
        {
            if seen.contains(&confinement) {
                return Err(ControlError::Internal(format!(
                    "the ACP confinement measurement contains duplicate `{confinement}` rows"
                )));
            }
            seen.push(confinement);
        }

        let measurements = ConfinementMeasurements {
            supported: Confinement::ALL
                .into_iter()
                .filter_map(|candidate| {
                    supported
                        .iter()
                        .find(|(confinement, _)| *confinement == candidate)
                        .cloned()
                })
                .collect(),
            unsupported: Confinement::ALL
                .into_iter()
                .filter_map(|candidate| {
                    unsupported
                        .iter()
                        .find(|(confinement, _)| *confinement == candidate)
                        .cloned()
                })
                .collect(),
        };
        let selected_model = self
            .store
            .as_ref()
            .map(|store| store.selected_model(&session))
            .transpose()
            .map_err(|error| ControlError::Internal(error.to_string()))?
            .flatten();
        let mut sessions = self.lock_sessions()?;
        let seat = sessions.entry(session).or_default();
        if seat.confinement.is_some() {
            return Err(ControlError::Internal(
                "the ACP confinement measurements for this session are already pinned".to_owned(),
            ));
        }
        seat.chosen.model = selected_model.or_else(|| seat.chosen.model.clone());
        seat.confinement = Some(measurements);
        Ok(())
    }

    /// Read the immutable observation paired with one supported picker value.
    pub fn observation(
        &self,
        session: SessionId,
        confinement: Confinement,
    ) -> Result<ExecObservation, ControlError> {
        let sessions = self.lock_sessions()?;
        let seat = sessions.get(&session).ok_or_else(Self::uninitialized)?;
        if !confinement.within(self.session_ceiling(seat)) {
            return Err(self.ceiling_refusal());
        }
        let measurements = seat.confinement.as_ref().ok_or_else(Self::uninitialized)?;
        if let Some(observation) = measurements.observation(confinement) {
            return Ok(observation.clone());
        }
        if let Some(missing) = measurements.refusal(confinement) {
            return Err(ControlError::MissingCapabilities {
                control: "sandbox",
                value: confinement.as_str().to_owned(),
                missing: Box::new(missing.clone()),
            });
        }
        Err(ControlError::Unavailable {
            control: "sandbox",
            value: confinement.as_str().to_owned(),
        })
    }

    fn ceiling_refusal(&self) -> ControlError {
        ControlError::Refused {
            control: "sandbox",
            why: format!(
                "this connection was started at `{}`, and a session can only be confined further than that, never less",
                self.sandbox_ceiling
            ),
        }
    }

    /// Every model this connection can actually run, current one included
    /// when selection has resolved one.
    ///
    /// A configured model remains present even when discovery never saw it: a
    /// host that cannot re-select what the session started with would be
    /// looking at a picker that disagrees with the run.
    fn models(&self, current: Option<&ModelRef>) -> Vec<ModelRef> {
        let mut models: Vec<_> = current.cloned().into_iter().collect();
        if let Some(catalogue) = &self.catalogue {
            for model in catalogue
                .models
                .iter()
                .filter(|entry| matches!(entry.availability, ModelAvailability::Available))
                .map(|entry| ModelRef(entry.slug.to_string()))
            {
                if !models.contains(&model) {
                    models.push(model);
                }
            }
        }
        models
    }

    /// Exactly the efforts this model accepts, or `None` when nobody knows.
    ///
    /// `None` is not "all seven": a model whose reasoning support is unknown,
    /// or that discovery never saw, has nobody vouching for any level, and an
    /// effort the provider rejects would break the next generation. This is
    /// the list a change is validated against, so it must never be widened to
    /// accommodate what is already set.
    fn accepted(&self, model: &ModelRef) -> Option<Vec<ThinkingLevel>> {
        reasoning_levels(self.catalogue.as_ref()?, &model.0).ok()
    }

    fn resolved_effort(
        &self,
        model: &ModelRef,
        explicit: Option<ThinkingLevel>,
    ) -> Option<ThinkingLevel> {
        let row = self
            .catalogue
            .as_ref()?
            .models
            .iter()
            .find(|entry| entry.slug.to_string() == model.0)?;
        row.capabilities.resolve_reasoning_effort(explicit).ok()
    }

    /// What the effort picker offers for this model, with the value in force
    /// always present.
    ///
    /// Distinct from [`Self::accepted`] on purpose. Configuration can seed a
    /// session on an effort its model does not accept; dropping that value
    /// from the offer would leave a host looking at a picker whose current
    /// value is not one of the choices, which no client can render honestly.
    /// Showing it is not endorsing it — [`Self::stage`] still refuses to move
    /// *to* a level the model rejects.
    fn efforts(
        &self,
        model: Option<&ModelRef>,
        current: Option<ThinkingLevel>,
    ) -> Vec<ThinkingLevel> {
        let Some(model) = model else {
            return Vec::new();
        };
        let Some(mut efforts) = self.accepted(model) else {
            return current.into_iter().collect();
        };
        if let Some(current) = current
            && !efforts.contains(&current)
        {
            efforts.push(current);
            efforts.sort_unstable();
        }
        efforts
    }

    fn compose(&self, seat: &SessionSeat) -> Result<SessionControlState, ControlError> {
        let measured = seat.confinement.as_ref().ok_or_else(Self::uninitialized)?;
        let chosen = &seat.chosen;
        let profile = seat.profile.as_ref().map(|profile| &profile.effective);
        let model = chosen.model.clone().or_else(|| {
            profile.map_or_else(|| self.model.clone(), |effective| effective.model.clone())
        });
        let explicit_effort = chosen
            .effort
            .or_else(|| profile.map_or(self.effort, |effective| effective.thinking));
        let effort = model
            .as_ref()
            .and_then(|model| explicit_effort.or_else(|| self.resolved_effort(model, None)));
        let mode = chosen.mode.unwrap_or_else(|| {
            profile.map_or(self.mode, |effective| {
                effective.mode.unwrap_or(Mode::Default)
            })
        });
        Ok(SessionControlState {
            model: ControlSelect {
                available: self.models(model.as_ref()),
                current: model.clone(),
                chosen: chosen.model.clone(),
            },
            effort: ControlSelect {
                available: self.efforts(model.as_ref(), effort),
                current: effort,
                chosen: chosen.effort,
            },
            mode: ControlSelect {
                available: self
                    .modes(profile.map_or(self.judge, |effective| effective.judge.is_some()))
                    .into_iter()
                    .filter(|mode| {
                        self.session_max_mode(seat)
                            .is_none_or(|maximum| mode.within(maximum))
                    })
                    .collect(),
                current: Some(mode),
                chosen: chosen.mode,
            },
            sandbox: ControlSelect {
                available: self.sandboxes(measured, self.session_ceiling(seat)),
                current: Some(chosen.sandbox.unwrap_or(self.session_ceiling(seat))),
                chosen: chosen.sandbox,
            },
        })
    }

    /// Every measured confinement at least as strong as the ceiling.
    ///
    /// An unsupported configured ceiling remains the effective current value
    /// so the next prompt can refuse it with its typed measurement. It is not
    /// silently replaced by a different sandbox merely to keep the current
    /// value inside the picker offer.
    fn sandboxes(
        &self,
        measured: &ConfinementMeasurements,
        ceiling: Confinement,
    ) -> Vec<Confinement> {
        measured
            .supported
            .iter()
            .map(|(confinement, _)| *confinement)
            .filter(|confinement| confinement.within(ceiling))
            .collect()
    }

    /// Auto is offered only where it can work: it delegates to a judge model,
    /// and without one the run refuses at startup rather than mid-turn.
    fn modes(&self, judge: bool) -> Vec<Mode> {
        HOST_MODES
            .into_iter()
            .filter(|mode| judge || *mode != Mode::Auto)
            .collect()
    }
}

impl SessionControls for AcpSessionControls {
    fn state(&self, session: SessionId) -> Result<SessionControlState, ControlError> {
        let sessions = self.lock_sessions()?;
        let seat = sessions.get(&session).ok_or_else(Self::uninitialized)?;
        self.compose(seat)
    }

    fn stage(
        &self,
        session: SessionId,
        change: SessionControlChange,
    ) -> Result<SessionControlState, ControlError> {
        let mut sessions = self.lock_sessions()?;
        let seat = sessions.get_mut(&session).ok_or_else(Self::uninitialized)?;
        let measured = seat.confinement.as_ref().ok_or_else(Self::uninitialized)?;
        let max_mode = self.session_max_mode(seat);
        let sandbox_ceiling = self.session_ceiling(seat);
        let profile = seat.profile.as_ref().map(|profile| &profile.effective);
        let default_model =
            profile.map_or(self.model.as_ref(), |effective| effective.model.as_ref());
        let default_effort = profile.map_or(self.effort, |effective| effective.thinking);
        let judge = profile.map_or(self.judge, |effective| effective.judge.is_some());
        let chosen = &mut seat.chosen;
        match change {
            SessionControlChange::Model(model) => {
                // Re-validating here is not distrust of the adapter: the seat
                // is the only thing that knows the catalogue, and a model
                // change can leave the effort behind on a model that does not
                // accept it.
                let explicit_effort = chosen.effort.or(default_effort);
                if explicit_effort.is_some()
                    && self.resolved_effort(&model, explicit_effort).is_none()
                {
                    return Err(ControlError::Refused {
                        control: "model",
                        why: format!(
                            "`{model}` does not accept the explicit reasoning effort in force"
                        ),
                    });
                }
                if let Some(store) = &self.store {
                    store
                        .record_model_selection(&session, model.clone())
                        .map_err(|error| ControlError::Internal(error.to_string()))?;
                }
                chosen.model = Some(model);
            }
            SessionControlChange::Effort(effort) => {
                let model = chosen.model.as_ref().or(default_model);
                if model
                    .and_then(|model| self.resolved_effort(model, Some(effort)))
                    .is_none()
                {
                    return Err(ControlError::Unavailable {
                        control: "effort",
                        value: effort.as_str().to_owned(),
                    });
                }
                chosen.effort = Some(effort);
            }
            SessionControlChange::Mode(mode) => {
                if let Some(maximum) = max_mode
                    && !mode.within(maximum)
                {
                    return Err(ControlError::Refused {
                        control: "mode",
                        why: format!("mode {mode} exceeds session max_mode {maximum}"),
                    });
                }
                if !self.modes(judge).contains(&mode) {
                    return Err(ControlError::Refused {
                        control: "mode",
                        why: match mode {
                            Mode::Auto => {
                                "`auto` needs a configured `[permissions] judge`".to_owned()
                            }
                            other => format!("`{other}` is not offered on this connection"),
                        },
                    });
                }
                chosen.mode = Some(mode);
            }
            SessionControlChange::Sandbox(confinement) => {
                // `Refused`, not `Unavailable`. A weaker confinement is a real
                // value that this connection may not reach, and telling the
                // host to "pick from the list" would suggest the list is
                // merely incomplete rather than deliberately capped.
                if !confinement.within(sandbox_ceiling) {
                    return Err(self.ceiling_refusal());
                }
                if let Some(missing) = measured.refusal(confinement) {
                    return Err(ControlError::MissingCapabilities {
                        control: "sandbox",
                        value: confinement.as_str().to_owned(),
                        missing: Box::new(missing.clone()),
                    });
                }
                if measured.observation(confinement).is_none() {
                    return Err(ControlError::Unavailable {
                        control: "sandbox",
                        value: confinement.as_str().to_owned(),
                    });
                }
                chosen.sandbox = Some(confinement);
            }
        }
        self.compose(seat)
    }
}
