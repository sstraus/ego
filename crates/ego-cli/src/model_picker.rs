//! Live, terminal-independent authority for the shared model picker action.

use std::sync::Arc;

use ego_ai::catalogue::{
    ModelAvailability, ModelAvailabilityCause, ModelCatalogueEntry, ModelCatalogueSnapshot,
    ModelCatalogueSourceStatus, ModelSlug,
};
use ego_core::config::{
    ModelRef, UserConfigError, UserConfigMutation, UserConfigPublication, UserConfigStore,
};
use ego_core::generator::capabilities::ReasoningEffortSupport;
use ego_core::generator::types::ThinkingLevel;

use crate::model::GenerationSelection;
use crate::repl::actions::{ActionId, ActionSink, LocalAction, LocalActionOutcome};
use crate::repl::selector::{Selector, SelectorKey, SelectorKind, SelectorOption, SelectorValue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    Cancelled,
    SessionInstalled,
    Persisted(UserConfigPublication),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerFocus {
    Model,
    Reasoning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerInput {
    Up,
    Down,
    Number(usize),
    Enter,
    SessionOnly,
    Escape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerInputOutcome {
    Consumed,
    FocusChanged(PickerFocus),
    Closed(PickerOutcome),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPickerView {
    pub open: bool,
    pub focus: PickerFocus,
    pub current_id: Option<String>,
    pub selected_model: Option<String>,
    pub models: Vec<ModelPickerRow>,
    pub reasoning: Vec<ThinkingLevel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPickerRow {
    pub slug: ModelSlug,
    pub max_context_tokens: Option<u64>,
    pub reasoning: ReasoningEffortSupport,
    pub availability: ModelAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerError {
    Closed,
    UnknownModel {
        slug: String,
    },
    Unavailable {
        slug: String,
        cause: ModelAvailabilityCause,
        message: String,
    },
    UnknownReasoningSupport {
        slug: String,
    },
    UnsupportedReasoning {
        slug: String,
        thinking: ThinkingLevel,
    },
    NoSelection,
    NoSession,
    SelectionRead(String),
    Persistence(UserConfigError),
    SessionInstall(String),
}

impl std::fmt::Display for PickerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => f.write_str("the model picker is closed"),
            Self::UnknownModel { slug } => {
                write!(f, "model `{slug}` is absent from the current catalogue")
            }
            Self::Unavailable {
                slug,
                cause,
                message,
            } => write!(f, "model `{slug}` is unavailable ({cause:?}): {message}"),
            Self::UnknownReasoningSupport { slug } => {
                write!(f, "model `{slug}` has no current reasoning-effort facts")
            }
            Self::UnsupportedReasoning { slug, thinking } => {
                write!(f, "model `{slug}` does not accept `{thinking:?}` reasoning")
            }
            Self::NoSelection => f.write_str("no model selection was made"),
            Self::NoSession => f.write_str("the model picker has no active session"),
            Self::SelectionRead(message) => {
                write!(
                    f,
                    "the current model selection could not be read: {message}"
                )
            }
            Self::Persistence(error) => write!(f, "user defaults were not persisted: {error}"),
            Self::SessionInstall(message) => {
                write!(f, "the session selection was not installed: {message}")
            }
        }
    }
}

impl std::error::Error for PickerError {}

#[derive(Debug, Clone)]
struct Chosen {
    model: String,
    thinking: ThinkingLevel,
}

#[derive(Debug, Clone)]
enum ModalState {
    Model(Selector),
    Reasoning { model: String, selector: Selector },
}

pub struct ModelPicker {
    snapshot: Arc<ModelCatalogueSnapshot>,
    selection: Option<Arc<GenerationSelection>>,
    config_store: Option<Arc<dyn UserConfigStore>>,
    chosen: Option<Chosen>,
    open: bool,
    modal: Option<ModalState>,
    diagnostic: Option<PickerError>,
}

impl ModelPicker {
    pub fn new(snapshot: Arc<ModelCatalogueSnapshot>) -> Self {
        Self {
            snapshot,
            selection: None,
            config_store: None,
            chosen: None,
            open: false,
            modal: None,
            diagnostic: None,
        }
    }

    pub fn for_selection(
        snapshot: Arc<ModelCatalogueSnapshot>,
        selection: Arc<GenerationSelection>,
        config_store: Arc<dyn UserConfigStore>,
    ) -> Self {
        Self {
            snapshot,
            selection: Some(selection),
            config_store: Some(config_store),
            chosen: None,
            open: false,
            modal: None,
            diagnostic: None,
        }
    }

    pub fn snapshot(&self) -> &Arc<ModelCatalogueSnapshot> {
        &self.snapshot
    }

    pub fn entries(&self) -> &[ModelCatalogueEntry] {
        &self.snapshot.models
    }

    pub fn sources(&self) -> &[ModelCatalogueSourceStatus] {
        &self.snapshot.sources
    }

    pub fn row_count(&self) -> usize {
        self.snapshot.models.len()
    }

    pub fn slugs(&self) -> Vec<ModelSlug> {
        self.snapshot
            .models
            .iter()
            .map(|row| row.slug.clone())
            .collect()
    }

    pub fn reasoning_efforts(&self) -> Vec<ReasoningEffortSupport> {
        self.snapshot
            .models
            .iter()
            .map(|row| row.capabilities.reasoning_efforts.clone())
            .collect()
    }

    pub fn can_select(&self, slug: &ModelSlug) -> bool {
        self.snapshot.models.iter().any(|row| {
            &row.slug == slug && matches!(&row.availability, ModelAvailability::Available)
        })
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The typed cause behind the latest action-level refusal, if any.
    pub fn diagnostic(&self) -> Option<&PickerError> {
        self.diagnostic.as_ref()
    }

    pub fn view(&self) -> ModelPickerView {
        let (focus, selector, selected_model, reasoning) = match &self.modal {
            Some(ModalState::Model(selector)) => {
                (PickerFocus::Model, Some(selector), None, Vec::new())
            }
            Some(ModalState::Reasoning { model, selector }) => (
                PickerFocus::Reasoning,
                Some(selector),
                Some(model.clone()),
                self.reasoning_levels(model).unwrap_or_default(),
            ),
            None => (PickerFocus::Model, None, None, Vec::new()),
        };
        ModelPickerView {
            open: self.open,
            focus,
            current_id: selector.and_then(Selector::current_id).map(str::to_owned),
            selected_model,
            models: self
                .snapshot
                .models
                .iter()
                .map(|row| ModelPickerRow {
                    slug: row.slug.clone(),
                    max_context_tokens: row.capabilities.max_context_tokens,
                    reasoning: row.capabilities.reasoning_efforts.clone(),
                    availability: row.availability.clone(),
                })
                .collect(),
            reasoning,
        }
    }

    fn stage_choice(&mut self, slug: &str, thinking: ThinkingLevel) -> Result<(), PickerError> {
        self.validate_choice(slug, thinking)?;
        self.chosen = Some(Chosen {
            model: slug.to_string(),
            thinking,
        });
        Ok(())
    }

    pub fn input(&mut self, input: PickerInput) -> Result<PickerInputOutcome, PickerError> {
        if !self.open {
            return Err(PickerError::Closed);
        }
        if input == PickerInput::Escape {
            self.close();
            return Ok(PickerInputOutcome::Closed(PickerOutcome::Cancelled));
        }
        match self.modal.take().ok_or(PickerError::Closed)? {
            ModalState::Model(mut selector) => {
                if input == PickerInput::SessionOnly {
                    self.modal = Some(ModalState::Model(selector));
                    return Err(PickerError::NoSelection);
                }
                let key = selector_key(input).ok_or(PickerError::NoSelection)?;
                selector.key(key);
                if input != PickerInput::Enter {
                    self.modal = Some(ModalState::Model(selector));
                    return Ok(PickerInputOutcome::Consumed);
                }
                let Some(model) = selected_id(&selector) else {
                    self.modal = Some(ModalState::Model(selector));
                    return Err(PickerError::NoSelection);
                };
                let reasoning = match self.reasoning_levels(&model) {
                    Ok(reasoning) => reasoning,
                    Err(error) => {
                        self.modal = Some(ModalState::Model(selector));
                        return Err(error);
                    }
                };
                self.modal = Some(ModalState::Reasoning {
                    model,
                    selector: reasoning_selector(&reasoning),
                });
                Ok(PickerInputOutcome::FocusChanged(PickerFocus::Reasoning))
            }
            ModalState::Reasoning {
                model,
                mut selector,
            } => {
                let commit = match input {
                    PickerInput::Enter => Some(true),
                    PickerInput::SessionOnly => Some(false),
                    _ => None,
                };
                let key = selector_key(input).ok_or(PickerError::NoSelection)?;
                selector.key(key);
                if let Some(commit) = commit {
                    let Some(thinking) = selected_id(&selector).and_then(|id| parse_thinking(&id))
                    else {
                        self.modal = Some(ModalState::Reasoning { model, selector });
                        return Err(PickerError::NoSelection);
                    };
                    if let Err(error) = self.stage_choice(&model, thinking) {
                        self.modal = Some(ModalState::Reasoning { model, selector });
                        return Err(error);
                    }
                    return match self.commit(commit) {
                        Ok(outcome) => Ok(PickerInputOutcome::Closed(outcome)),
                        Err(error) => {
                            self.modal = Some(ModalState::Reasoning { model, selector });
                            Err(error)
                        }
                    };
                }
                self.modal = Some(ModalState::Reasoning { model, selector });
                Ok(PickerInputOutcome::Consumed)
            }
        }
    }

    fn commit(&mut self, persist: bool) -> Result<PickerOutcome, PickerError> {
        let chosen = self.chosen.clone().ok_or(PickerError::NoSelection)?;
        if !persist {
            self.install(&chosen)?;
            self.close();
            return Ok(PickerOutcome::SessionInstalled);
        }
        let store = self.config_store.as_ref().ok_or(PickerError::NoSession)?;
        let publication = store
            .mutate(UserConfigMutation::ModelAndThinking {
                model: chosen.model.clone(),
                thinking: chosen.thinking,
            })
            .map_err(PickerError::Persistence)?;
        self.install(&chosen)?;
        self.close();
        Ok(PickerOutcome::Persisted(publication))
    }

    pub fn replace_snapshot(
        &mut self,
        snapshot: Arc<ModelCatalogueSnapshot>,
    ) -> LocalActionOutcome {
        self.snapshot = snapshot;
        self.chosen = None;
        self.diagnostic = None;
        if self.open {
            self.modal = Some(ModalState::Model(model_selector(&self.snapshot)));
        }
        LocalActionOutcome::Completed
    }

    fn close(&mut self) {
        self.open = false;
        self.chosen = None;
        self.modal = None;
        self.diagnostic = None;
    }

    fn open(&mut self) {
        self.open = true;
        self.chosen = None;
        self.modal = Some(ModalState::Model(model_selector(&self.snapshot)));
        self.diagnostic = None;
    }

    fn open_reasoning(&mut self) -> Result<(), PickerError> {
        let selection = self.selection.as_ref().ok_or(PickerError::NoSession)?;
        let current = selection
            .snapshot()
            .map_err(|error| PickerError::SelectionRead(error.to_string()))?;
        let model = current.identity.0;
        let reasoning = self.reasoning_levels(&model)?;
        let mut selector = reasoning_selector(&reasoning);
        if let Some(index) = reasoning
            .iter()
            .position(|level| *level == current.thinking)
        {
            selector.key(SelectorKey::Number(index + 1));
        }
        self.open = true;
        self.chosen = None;
        self.modal = Some(ModalState::Reasoning { model, selector });
        self.diagnostic = None;
        Ok(())
    }

    fn ensure_model_available(&self, slug: &str) -> Result<&ModelCatalogueEntry, PickerError> {
        let row = self
            .snapshot
            .models
            .iter()
            .find(|row| row.slug.to_string() == slug)
            .ok_or_else(|| PickerError::UnknownModel {
                slug: slug.to_string(),
            })?;
        if let ModelAvailability::Unavailable { cause, message } = &row.availability {
            return Err(PickerError::Unavailable {
                slug: slug.to_string(),
                cause: *cause,
                message: message.clone(),
            });
        }
        Ok(row)
    }

    fn reasoning_levels(&self, slug: &str) -> Result<Vec<ThinkingLevel>, PickerError> {
        let row = self.ensure_model_available(slug)?;
        match &row.capabilities.reasoning_efforts {
            ReasoningEffortSupport::Unknown => Err(PickerError::UnknownReasoningSupport {
                slug: slug.to_string(),
            }),
            ReasoningEffortSupport::Any => Ok(ALL_THINKING.to_vec()),
            ReasoningEffortSupport::Only(levels) => Ok(levels.iter().copied().collect()),
        }
    }

    fn validate_choice(&self, slug: &str, thinking: ThinkingLevel) -> Result<(), PickerError> {
        let levels = self.reasoning_levels(slug)?;
        if !levels.contains(&thinking) {
            return Err(PickerError::UnsupportedReasoning {
                slug: slug.to_string(),
                thinking,
            });
        }
        Ok(())
    }

    fn install(&self, chosen: &Chosen) -> Result<(), PickerError> {
        let selection = self.selection.as_ref().ok_or(PickerError::NoSession)?;
        selection
            .install(ModelRef(chosen.model.clone()), chosen.thinking)
            .map_err(PickerError::SessionInstall)
    }
}

impl ActionSink for ModelPicker {
    fn invoke(&mut self, action: LocalAction) -> LocalActionOutcome {
        if !matches!(action.id, ActionId::Model | ActionId::Thinking) {
            return LocalActionOutcome::Refused {
                reason: "the model picker handles only model selection actions",
            };
        }
        if self.open {
            return LocalActionOutcome::InFlight;
        }
        match action.id {
            ActionId::Model => {
                self.open();
                LocalActionOutcome::Completed
            }
            ActionId::Thinking => match self.open_reasoning() {
                Ok(()) => LocalActionOutcome::Completed,
                Err(error) => {
                    self.diagnostic = Some(error);
                    LocalActionOutcome::Refused {
                        reason: "the current model has no selectable reasoning authority",
                    }
                }
            },
            _ => unreachable!("non-picker actions were refused above"),
        }
    }
}

const ALL_THINKING: [ThinkingLevel; 7] = [
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];

fn model_selector(snapshot: &ModelCatalogueSnapshot) -> Selector {
    Selector::with_options(
        SelectorKind::Single,
        snapshot.models.iter().map(|row| {
            let slug = row.slug.to_string();
            SelectorOption::new(slug.clone(), slug)
        }),
    )
}

fn reasoning_selector(levels: &[ThinkingLevel]) -> Selector {
    Selector::with_options(
        SelectorKind::Single,
        levels.iter().map(|level| {
            let id = thinking_id(*level);
            SelectorOption::new(id, id)
        }),
    )
}

fn selector_key(input: PickerInput) -> Option<SelectorKey> {
    match input {
        PickerInput::Up => Some(SelectorKey::Up),
        PickerInput::Down => Some(SelectorKey::Down),
        PickerInput::Number(number) => Some(SelectorKey::Number(number)),
        PickerInput::Enter | PickerInput::SessionOnly => Some(SelectorKey::Enter),
        PickerInput::Escape => None,
    }
}

fn selected_id(selector: &Selector) -> Option<String> {
    match selector.result()? {
        SelectorValue::Single(id) => Some(id.clone()),
        SelectorValue::Multiple(_) | SelectorValue::FreeText(_) => None,
    }
}

fn thinking_id(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn parse_thinking(value: &str) -> Option<ThinkingLevel> {
    ALL_THINKING
        .iter()
        .copied()
        .find(|level| thinking_id(*level) == value)
}
