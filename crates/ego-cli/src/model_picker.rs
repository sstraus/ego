//! Live, terminal-independent authority for the shared model picker action.

use std::sync::Arc;

use ego_ai::catalogue::{
    ModelAvailability, ModelAvailabilityCause, ModelCatalogueEntry, ModelCatalogueSnapshot,
    ModelCatalogueSourceStatus, ModelSlug,
};
use ego_core::config::{ModelRef, UserConfigStore};
use ego_core::generator::capabilities::ReasoningEffortSupport;
use ego_core::generator::types::ThinkingLevel;

use crate::model::GenerationSelection;
use crate::repl::actions::{ActionId, ActionSink, LocalAction, LocalActionOutcome};
use crate::repl::selector::{Selector, SelectorKey, SelectorKind, SelectorOption, SelectorValue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    Cancelled,
    ModelSwitch(ModelSwitchRequest),
}

/// A validated selection staged for the startup or asynchronous run owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSwitchRequest {
    target: ModelRef,
    thinking: ThinkingLevel,
    persistent: bool,
}

impl ModelSwitchRequest {
    /// A startup selection that must become the next user default.
    #[cfg(test)]
    pub(crate) fn persistent(target: ModelRef, thinking: ThinkingLevel) -> Self {
        Self {
            target,
            thinking,
            persistent: true,
        }
    }

    pub fn target(&self) -> ModelRef {
        self.target.clone()
    }

    pub fn thinking(&self) -> ThinkingLevel {
        self.thinking
    }

    pub fn is_persistent(&self) -> bool {
        self.persistent
    }
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
    /// A printable key. Filters the model list; at the reasoning step only
    /// `s` means anything.
    Char(char),
    Backspace,
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
    /// What the operator has typed to narrow `models`.
    pub filter: String,
    /// The model the session is generating with right now, as opposed to the
    /// highlighted row.
    pub current_model: Option<String>,
    /// The effort in force, only while the reasoning step is for that model.
    pub current_reasoning: Option<ThinkingLevel>,
    pub models: Vec<ModelPickerRow>,
    pub reasoning: Vec<ThinkingLevel>,
    /// Startup has no active session to scope a selection to.
    pub(crate) persistent_only: bool,
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
    PersistenceRequired,
    SelectionRead(String),
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
            Self::PersistenceRequired => {
                f.write_str("the startup model selection must be saved as the user default")
            }
            Self::SelectionRead(message) => {
                write!(
                    f,
                    "the current model selection could not be read: {message}"
                )
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

/// The selection in force when the picker opened.
#[derive(Debug, Clone)]
struct CurrentSelection {
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
    startup: bool,
    chosen: Option<Chosen>,
    current: Option<CurrentSelection>,
    filter: String,
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
            startup: false,
            chosen: None,
            current: None,
            filter: String::new(),
            open: false,
            modal: None,
            diagnostic: None,
        }
    }

    /// An already-open picker whose result establishes the first user default.
    pub(crate) fn for_startup(snapshot: Arc<ModelCatalogueSnapshot>) -> Self {
        Self {
            modal: Some(ModalState::Model(model_selector(&snapshot, "", None))),
            snapshot,
            selection: None,
            config_store: None,
            startup: true,
            chosen: None,
            current: None,
            filter: String::new(),
            open: true,
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
            startup: false,
            chosen: None,
            current: None,
            filter: String::new(),
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
            current_reasoning: selected_model.as_deref().and_then(|model| {
                self.current
                    .as_ref()
                    .filter(|current| current.model == model)
                    .map(|current| current.thinking)
            }),
            selected_model,
            filter: self.filter.clone(),
            current_model: self.current.as_ref().map(|current| current.model.clone()),
            models: self
                .ordered_rows()
                .into_iter()
                .map(|row| ModelPickerRow {
                    slug: row.slug.clone(),
                    max_context_tokens: row.capabilities.max_context_tokens,
                    reasoning: row.capabilities.reasoning_efforts.clone(),
                    availability: row.availability.clone(),
                })
                .collect(),
            reasoning,
            persistent_only: self.startup,
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
        let input = match (input, &self.modal) {
            (PickerInput::Char('s' | 'S'), Some(ModalState::Reasoning { .. })) => {
                PickerInput::SessionOnly
            }
            (
                PickerInput::Char(_) | PickerInput::Backspace,
                Some(ModalState::Reasoning { .. }),
            ) => return Ok(PickerInputOutcome::Consumed),
            (input, _) => input,
        };
        match self.modal.take().ok_or(PickerError::Closed)? {
            ModalState::Model(mut selector) => {
                // Digits select by number only while there is no filter: a
                // slug such as `gpt-5.6` needs them as text once typing began.
                let typed = match input {
                    PickerInput::Char(character) => Some(character),
                    PickerInput::Number(number) if !self.filter.is_empty() => {
                        char::from_digit(u32::try_from(number).unwrap_or(u32::MAX), 10)
                    }
                    _ => None,
                };
                if let Some(character) = typed {
                    self.filter.push(character);
                    self.modal = Some(ModalState::Model(self.model_selector()));
                    return Ok(PickerInputOutcome::Consumed);
                }
                if input == PickerInput::Backspace {
                    // An empty filter has nothing to erase and keeps its cursor.
                    self.modal = Some(ModalState::Model(if self.filter.pop().is_some() {
                        self.model_selector()
                    } else {
                        selector
                    }));
                    return Ok(PickerInputOutcome::Consumed);
                }
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
                let (reasoning, default) = match reasoning_configuration(&self.snapshot, &model) {
                    Ok(configuration) => configuration,
                    Err(error) => {
                        self.modal = Some(ModalState::Model(selector));
                        return Err(error);
                    }
                };
                self.modal = Some(ModalState::Reasoning {
                    model,
                    selector: reasoning_selector(&reasoning, default),
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
        if self.startup && !persist {
            return Err(PickerError::PersistenceRequired);
        }
        if !self.startup && (self.selection.is_none() || persist && self.config_store.is_none()) {
            return Err(PickerError::NoSession);
        }
        let request = ModelSwitchRequest {
            target: ModelRef(chosen.model),
            thinking: chosen.thinking,
            persistent: persist,
        };
        self.close();
        Ok(PickerOutcome::ModelSwitch(request))
    }

    pub fn replace_snapshot(
        &mut self,
        snapshot: Arc<ModelCatalogueSnapshot>,
    ) -> LocalActionOutcome {
        self.snapshot = snapshot;
        self.chosen = None;
        self.diagnostic = None;
        if self.open {
            self.modal = Some(ModalState::Model(self.model_selector()));
        }
        LocalActionOutcome::Completed
    }

    fn close(&mut self) {
        self.open = false;
        self.chosen = None;
        self.filter.clear();
        self.modal = None;
        self.diagnostic = None;
    }

    fn open(&mut self) {
        self.open = true;
        self.chosen = None;
        self.filter.clear();
        // An unreadable selection leaves the list without a current row
        // rather than keeping the operator from choosing a model.
        self.current = self.selection.as_ref().and_then(|selection| {
            selection.snapshot().ok().map(|current| CurrentSelection {
                model: current.identity.0,
                thinking: current.thinking,
            })
        });
        self.modal = Some(ModalState::Model(self.model_selector()));
        self.diagnostic = None;
    }

    fn model_selector(&self) -> Selector {
        model_selector(
            &self.snapshot,
            &self.filter,
            self.current.as_ref().map(|current| current.model.as_str()),
        )
    }

    fn open_reasoning(&mut self) -> Result<(), PickerError> {
        let selection = self.selection.as_ref().ok_or(PickerError::NoSession)?;
        let current = selection
            .snapshot()
            .map_err(|error| PickerError::SelectionRead(error.to_string()))?;
        let model = current.identity.0;
        let reasoning = self.reasoning_levels(&model)?;
        let selector = reasoning_selector(&reasoning, Some(current.thinking));
        self.current = Some(CurrentSelection {
            model: model.clone(),
            thinking: current.thinking,
        });
        self.open = true;
        self.chosen = None;
        self.filter.clear();
        self.modal = Some(ModalState::Reasoning { model, selector });
        self.diagnostic = None;
        Ok(())
    }

    fn reasoning_levels(&self, slug: &str) -> Result<Vec<ThinkingLevel>, PickerError> {
        reasoning_levels(&self.snapshot, slug)
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

    fn ordered_rows(&self) -> Vec<&ModelCatalogueEntry> {
        filtered_rows(&self.snapshot, &self.filter)
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

/// The catalogue row for a model an operator may actually pick.
///
/// Free rather than a picker method because the terminal picker is not the
/// only seat: an ACP host picks from the same catalogue and must be told no
/// for the same reasons, and two copies of "is this model usable" is exactly
/// how one of them ends up offering a model the other refuses.
pub(crate) fn available_entry<'a>(
    snapshot: &'a ModelCatalogueSnapshot,
    slug: &str,
) -> Result<&'a ModelCatalogueEntry, PickerError> {
    let row = snapshot
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

/// Every reasoning effort this model is known to accept.
///
/// An unknown support set is an error rather than "all seven": offering a
/// level the provider then rejects turns a picker into a way to break the
/// next generation.
pub(crate) fn reasoning_levels(
    snapshot: &ModelCatalogueSnapshot,
    slug: &str,
) -> Result<Vec<ThinkingLevel>, PickerError> {
    reasoning_configuration(snapshot, slug).map(|(levels, _)| levels)
}

fn reasoning_configuration(
    snapshot: &ModelCatalogueSnapshot,
    slug: &str,
) -> Result<(Vec<ThinkingLevel>, Option<ThinkingLevel>), PickerError> {
    let row = available_entry(snapshot, slug)?;
    let levels = match &row.capabilities.reasoning_efforts {
        ReasoningEffortSupport::Unknown => Err(PickerError::UnknownReasoningSupport {
            slug: slug.to_string(),
        }),
        ReasoningEffortSupport::Any => Ok(ALL_THINKING.to_vec()),
        ReasoningEffortSupport::Only(levels) => Ok(levels.iter().copied().collect()),
    }?;
    Ok((levels, row.capabilities.default_reasoning_effort))
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

/// The rows whose slug contains every whitespace-separated word of `filter`,
/// ignoring case, in the order the picker lists them.
fn filtered_rows<'a>(
    snapshot: &'a ModelCatalogueSnapshot,
    filter: &str,
) -> Vec<&'a ModelCatalogueEntry> {
    let filter = filter.to_lowercase();
    let words = filter.split_whitespace().collect::<Vec<_>>();
    ordered_rows(snapshot)
        .into_iter()
        .filter(|row| {
            let slug = row.slug.to_string().to_lowercase();
            words.iter().all(|word| slug.contains(word))
        })
        .collect()
}

/// The list for `filter`, with the cursor on the current model when it is
/// still listed and on the first row otherwise.
fn model_selector(
    snapshot: &ModelCatalogueSnapshot,
    filter: &str,
    current: Option<&str>,
) -> Selector {
    let slugs = filtered_rows(snapshot, filter)
        .into_iter()
        .map(|row| row.slug.to_string())
        .collect::<Vec<_>>();
    let current_row = current.and_then(|current| slugs.iter().position(|slug| slug == current));
    let mut selector = Selector::with_options(
        SelectorKind::Single,
        slugs
            .into_iter()
            .map(|slug| SelectorOption::new(slug.clone(), slug)),
    );
    if let Some(row) = current_row {
        selector.key(SelectorKey::Number(row + 1));
    }
    selector
}

fn ordered_rows(snapshot: &ModelCatalogueSnapshot) -> Vec<&ModelCatalogueEntry> {
    let mut rows = snapshot.models.iter().collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        availability_order(&left.availability)
            .cmp(&availability_order(&right.availability))
            .then_with(|| left.slug.cmp(&right.slug))
    });
    rows
}

fn availability_order(availability: &ModelAvailability) -> u8 {
    match availability {
        ModelAvailability::Available => 0,
        ModelAvailability::Unavailable { .. } => 1,
    }
}

fn reasoning_selector(levels: &[ThinkingLevel], selected: Option<ThinkingLevel>) -> Selector {
    let options = || {
        levels.iter().map(|level| {
            let id = thinking_id(*level);
            SelectorOption::new(id, id)
        })
    };
    let Some(index) =
        selected.and_then(|selected| levels.iter().position(|level| *level == selected))
    else {
        return Selector::with_options_unselected(SelectorKind::Single, options());
    };
    let mut selector = Selector::with_options(SelectorKind::Single, options());
    selector.key(SelectorKey::Number(index + 1));
    selector
}

fn selector_key(input: PickerInput) -> Option<SelectorKey> {
    match input {
        PickerInput::Up => Some(SelectorKey::Up),
        PickerInput::Down => Some(SelectorKey::Down),
        PickerInput::Number(number) => Some(SelectorKey::Number(number)),
        PickerInput::Enter | PickerInput::SessionOnly => Some(SelectorKey::Enter),
        PickerInput::Escape | PickerInput::Char(_) | PickerInput::Backspace => None,
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
