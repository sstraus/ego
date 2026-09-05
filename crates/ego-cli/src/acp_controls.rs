//! The ACP connection's operator seat for model, effort and mode.
//!
//! `2026-09-03-acp-host-control-is-negotiated-per-session.md` makes the
//! connected client the person in an ACP-owned turn. This is the other half
//! of that: the picker that person is offered, and what a pick actually does.
//!
//! Everything is answered from the same two sources the terminal picker uses
//! — the discovered model catalogue and the workspace's resolved
//! configuration — so a host cannot be offered a model `ego models` calls
//! unavailable, or an effort the provider would reject.

use std::collections::HashMap;
use std::sync::Mutex;

use ego_ai::catalogue::{ModelAvailability, ModelCatalogueSnapshot};
use ego_core::config::{Effective, Mode, ModelRef};
use ego_core::control::{
    ControlError, ControlSelect, SessionControlChange, SessionControlState, SessionControls,
};
use ego_core::event::id::SessionId;
use ego_core::generator::types::ThinkingLevel;

use crate::model_picker::reasoning_levels;

/// Every mode an ACP host may select.
///
/// `Mode::Yolo` is deliberately absent. The terminal seat only reaches it by
/// typing the word `yolo` into a confirmation, because it is the mode that
/// stops asking; ACP's `session/set_config_option` carries a value id and
/// nothing else, so serving it here would turn a two-step confirmation into
/// one click. A host that wants it configures it, where the choice is
/// written down and reviewable.
const HOST_MODES: [Mode; 4] = [Mode::Plan, Mode::Default, Mode::Edits, Mode::Auto];

/// What one ACP session's operator has picked, if anything.
#[derive(Debug, Clone, Default)]
struct Chosen {
    model: Option<ModelRef>,
    effort: Option<ThinkingLevel>,
    mode: Option<Mode>,
}

/// One ACP connection's generation controls.
#[derive(Debug)]
pub struct AcpSessionControls {
    model: ModelRef,
    effort: ThinkingLevel,
    mode: Mode,
    judge: bool,
    catalogue: Option<ModelCatalogueSnapshot>,
    sessions: Mutex<HashMap<SessionId, Chosen>>,
}

impl AcpSessionControls {
    /// Seed the seat from the workspace configuration this connection serves.
    ///
    /// The catalogue is optional because discovery is an egress that can
    /// fail: a connection that could not enumerate models still offers mode,
    /// and offers exactly one model — the configured one — rather than a list
    /// it cannot vouch for.
    pub fn new(effective: &Effective, catalogue: Option<ModelCatalogueSnapshot>) -> Self {
        Self::seeded(
            effective.model.clone().unwrap_or(ModelRef(String::new())),
            effective.thinking.unwrap_or(ThinkingLevel::Off),
            effective.mode.unwrap_or(Mode::Default),
            effective.judge.is_some(),
            catalogue,
        )
    }

    /// The three resolved values and whether a judge exists, without the
    /// rest of the configuration. This is the whole of what the seat needs,
    /// and saying so is what lets it be tested without building a workspace.
    pub fn seeded(
        model: ModelRef,
        effort: ThinkingLevel,
        mode: Mode,
        judge: bool,
        catalogue: Option<ModelCatalogueSnapshot>,
    ) -> Self {
        Self {
            model,
            effort,
            mode,
            judge,
            catalogue,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn chosen(&self, session: SessionId) -> Result<Chosen, ControlError> {
        Ok(self
            .sessions
            .lock()
            .map_err(|_| ControlError::Internal("the ACP control lock is poisoned".to_owned()))?
            .get(&session)
            .cloned()
            .unwrap_or_default())
    }

    /// Every model this connection can actually run, current one included.
    ///
    /// The configured model is always present even when discovery never saw
    /// it: a host that cannot re-select what the session started with would
    /// be looking at a picker that disagrees with the run.
    fn models(&self, current: &ModelRef) -> Vec<ModelRef> {
        let mut models = vec![current.clone()];
        if let Some(catalogue) = &self.catalogue {
            models.extend(
                catalogue
                    .models
                    .iter()
                    .filter(|entry| matches!(entry.availability, ModelAvailability::Available))
                    .map(|entry| ModelRef(entry.slug.to_string()))
                    .filter(|model| model != current),
            );
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

    /// What the effort picker offers for this model, with the value in force
    /// always present.
    ///
    /// Distinct from [`Self::accepted`] on purpose. Configuration can seed a
    /// session on an effort its model does not accept; dropping that value
    /// from the offer would leave a host looking at a picker whose current
    /// value is not one of the choices, which no client can render honestly.
    /// Showing it is not endorsing it — [`Self::stage`] still refuses to move
    /// *to* a level the model rejects.
    fn efforts(&self, model: &ModelRef, current: ThinkingLevel) -> Vec<ThinkingLevel> {
        let Some(mut efforts) = self.accepted(model) else {
            return vec![current];
        };
        if !efforts.contains(&current) {
            efforts.push(current);
            efforts.sort_unstable();
        }
        efforts
    }

    fn compose(&self, chosen: Chosen) -> SessionControlState {
        let model = chosen.model.clone().unwrap_or_else(|| self.model.clone());
        let effort = chosen.effort.unwrap_or(self.effort);
        let mode = chosen.mode.unwrap_or(self.mode);
        SessionControlState {
            model: ControlSelect {
                available: self.models(&model),
                current: model.clone(),
                chosen: chosen.model,
            },
            effort: ControlSelect {
                available: self.efforts(&model, effort),
                current: effort,
                chosen: chosen.effort,
            },
            mode: ControlSelect {
                available: self.modes(),
                current: mode,
                chosen: chosen.mode,
            },
        }
    }

    /// Auto is offered only where it can work: it delegates to a judge model,
    /// and without one the run refuses at startup rather than mid-turn.
    fn modes(&self) -> Vec<Mode> {
        HOST_MODES
            .into_iter()
            .filter(|mode| self.judge || *mode != Mode::Auto)
            .collect()
    }
}

impl SessionControls for AcpSessionControls {
    fn state(&self, session: SessionId) -> Result<SessionControlState, ControlError> {
        Ok(self.compose(self.chosen(session)?))
    }

    fn stage(
        &self,
        session: SessionId,
        change: SessionControlChange,
    ) -> Result<SessionControlState, ControlError> {
        let mut chosen = self.chosen(session)?;
        match change {
            SessionControlChange::Model(model) => {
                // Re-validating here is not distrust of the adapter: the seat
                // is the only thing that knows the catalogue, and a model
                // change can leave the effort behind on a model that does not
                // accept it.
                let effort = chosen.effort.unwrap_or(self.effort);
                if let Some(accepted) = self.accepted(&model)
                    && !accepted.contains(&effort)
                {
                    // The highest the new model will take, rather than the
                    // lowest: the operator asked for this much thinking and
                    // the model is the only reason they cannot have it.
                    chosen.effort = accepted.last().copied();
                }
                chosen.model = Some(model);
            }
            SessionControlChange::Effort(effort) => {
                let model = chosen.model.clone().unwrap_or_else(|| self.model.clone());
                // The offer, not `accepted`: re-selecting the value already in
                // force is always allowed, even where nobody can vouch for the
                // model's levels. Everything else has to be vouched for.
                if !self
                    .efforts(&model, chosen.effort.unwrap_or(self.effort))
                    .contains(&effort)
                {
                    return Err(ControlError::Unavailable {
                        control: "effort",
                        value: effort.as_str().to_owned(),
                    });
                }
                chosen.effort = Some(effort);
            }
            SessionControlChange::Mode(mode) => {
                if !self.modes().contains(&mode) {
                    return Err(ControlError::Refused {
                        control: "mode",
                        why: match mode {
                            Mode::Auto => {
                                "`auto` needs a configured `[permissions] judge`".to_owned()
                            }
                            Mode::Yolo => {
                                "`yolo` is set in configuration, never over the wire".to_owned()
                            }
                            other => format!("`{other}` is not offered on this connection"),
                        },
                    });
                }
                chosen.mode = Some(mode);
            }
        }
        self.sessions
            .lock()
            .map_err(|_| ControlError::Internal("the ACP control lock is poisoned".to_owned()))?
            .insert(session, chosen.clone());
        Ok(self.compose(chosen))
    }
}
