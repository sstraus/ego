//! Live, terminal-independent authority for the shared model picker action.

use std::sync::Arc;

use ego_ai::catalogue::{
    ModelAvailability, ModelAvailabilityCause, ModelCatalogueEntry, ModelCatalogueSnapshot,
    ModelCatalogueSourceStatus, ModelSlug,
};
use ego_core::config::{ModelRef, UserConfigStore};
use ego_core::generator::capabilities::{ReasoningEffortSupport, ReasoningSelectionError};
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
    connection: Option<String>,
}

impl ModelSwitchRequest {
    /// A startup selection that must become the next user default.
    #[cfg(test)]
    pub(crate) fn persistent(target: ModelRef, thinking: ThinkingLevel) -> Self {
        Self {
            target,
            thinking,
            persistent: true,
            connection: None,
        }
    }

    pub fn target(&self) -> ModelRef {
        self.target.clone()
    }

    pub(crate) fn connection(&self) -> Option<&str> {
        self.connection.as_deref()
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
    Account,
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
    /// Stable numbers offered at the account step, in display order. Never
    /// the opaque connection IDs behind them.
    pub account_numbers: Vec<u32>,
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
    NoAvailableMatch(String),
    AmbiguousMatch {
        query: String,
        matches: Vec<String>,
    },
    MissingReasoningDefault(String),
    NoSession,
    PersistenceRequired,
    SelectionRead(String),
    AccountChoiceRequired,
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
            Self::NoAvailableMatch(query) => write!(f, "no available model matches `{query}`"),
            Self::AmbiguousMatch { query, matches } => write!(
                f,
                "multiple available models match `{query}`: {}; use a longer name or /model",
                matches.join(", ")
            ),
            Self::MissingReasoningDefault(slug) => write!(
                f,
                "model `{slug}` has no stated reasoning default; use /model to choose reasoning"
            ),
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
            Self::AccountChoiceRequired => f.write_str(
                "multiple saved accounts match; select a numbered account in /model, or use --account N in a new session"
            ),
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
    Reasoning {
        model: String,
        selector: Selector,
    },
    /// The chosen model's vendor has more than one configured account and
    /// none is yet bound for this session; `numbers` are its stable account
    /// numbers, never the opaque connection IDs behind them.
    Account {
        model: String,
        selector: Selector,
        numbers: Vec<u32>,
    },
}

pub struct ModelPicker {
    snapshot: Arc<ModelCatalogueSnapshot>,
    live_accounts: Vec<crate::auth::SavedKeyAccount>,
    base_snapshot: Arc<ModelCatalogueSnapshot>,
    chosen_account: Option<String>,
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
            base_snapshot: Arc::clone(&snapshot),
            live_accounts: Vec::new(),
            chosen_account: None,
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
            base_snapshot: Arc::clone(&snapshot),
            live_accounts: Vec::new(),
            chosen_account: None,
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

    /// Ask for the missing startup effort without asking for the model again.
    pub(crate) fn for_startup_reasoning(
        snapshot: Arc<ModelCatalogueSnapshot>,
        model: String,
    ) -> Result<Self, PickerError> {
        let reasoning = reasoning_levels(&snapshot, &model)?;
        let mut picker = Self::for_startup(snapshot);
        picker.modal = Some(ModalState::Reasoning {
            model,
            selector: reasoning_selector(&reasoning, None),
        });
        Ok(picker)
    }

    pub fn for_selection(
        snapshot: Arc<ModelCatalogueSnapshot>,
        selection: Arc<GenerationSelection>,
        config_store: Arc<dyn UserConfigStore>,
    ) -> Self {
        Self {
            base_snapshot: Arc::clone(&snapshot),
            live_accounts: Vec::new(),
            chosen_account: None,
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

    /// Refresh the live display without admitting a credential. Startup and
    /// an already bound session cannot adopt another connection here.
    pub(crate) fn set_live_accounts(&mut self, accounts: Vec<crate::auth::SavedKeyAccount>) {
        self.live_accounts = accounts;
        let mut snapshot = self.base_snapshot.as_ref().clone();
        if self.selection.is_some() && !self.startup {
            for row in &mut snapshot.models {
                if matches!(
                    row.availability,
                    ModelAvailability::Unavailable {
                        cause: ModelAvailabilityCause::MissingCredential,
                        ..
                    }
                ) && !self.eligible_keys(&row.slug.provider).is_empty()
                {
                    row.availability = ModelAvailability::Available;
                }
            }
        }
        self.snapshot = Arc::new(snapshot);
        if matches!(self.modal, Some(ModalState::Model(_))) {
            self.modal = Some(ModalState::Model(self.model_selector()));
        }
    }

    fn eligible_keys(&self, route: &str) -> Vec<&crate::auth::SavedKeyAccount> {
        let pinned = self.selection.as_ref().and_then(|selection| {
            let current = selection.snapshot().ok()?;
            let (current_route, _) = current.identity.0.split_once('/')?;
            (current_route == route)
                .then(|| selection.connection())
                .flatten()
        });
        self.live_accounts
            .iter()
            .filter(|account| account.route == route && pinned.is_none_or(|id| account.id == id))
            .collect()
    }

    fn single_key(&self, route: &str) -> Result<Option<String>, PickerError> {
        match self.eligible_keys(route).as_slice() {
            [] => Ok(None),
            [account] => Ok(Some(account.id.clone())),
            _ => Err(PickerError::AccountChoiceRequired),
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

    /// Resolve only a unique selectable model. Use its stated reasoning
    /// default or carry the session effort only when the target supports it.
    pub fn resolve_fragment(&self, query: &str) -> Result<ModelSwitchRequest, PickerError> {
        let matches = filtered_rows(&self.snapshot, query)
            .into_iter()
            .filter(|row| matches!(&row.availability, ModelAvailability::Available))
            .collect::<Vec<_>>();
        let row = match matches.as_slice() {
            [] => return Err(PickerError::NoAvailableMatch(query.to_owned())),
            [row] => row,
            _ => {
                return Err(PickerError::AmbiguousMatch {
                    query: query.to_owned(),
                    matches: matches.iter().map(|row| row.slug.to_string()).collect(),
                });
            }
        };
        let slug = row.slug.to_string();
        let current_effort = self
            .selection
            .as_ref()
            .and_then(|selection| selection.snapshot().ok().map(|current| current.thinking));
        let thinking = row
            .capabilities
            .resolve_reasoning_effort(row.capabilities.default_reasoning_effort.or(current_effort))
            .map_err(|error| match error {
                ReasoningSelectionError::DefaultUnstated => {
                    PickerError::MissingReasoningDefault(slug.clone())
                }
                error => PickerError::SelectionRead(error.to_string()),
            })?;
        Ok(ModelSwitchRequest {
            target: ModelRef(slug),
            thinking,
            persistent: false,
            connection: self.single_key(&row.slug.provider)?,
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
        let (focus, selector, selected_model, reasoning, account_numbers) = match &self.modal {
            Some(ModalState::Model(selector)) => (
                PickerFocus::Model,
                Some(selector),
                None,
                Vec::new(),
                Vec::new(),
            ),
            Some(ModalState::Reasoning { model, selector }) => (
                PickerFocus::Reasoning,
                Some(selector),
                Some(model.clone()),
                self.reasoning_levels(model).unwrap_or_default(),
                Vec::new(),
            ),
            Some(ModalState::Account {
                model,
                selector,
                numbers,
            }) => (
                PickerFocus::Account,
                Some(selector),
                Some(model.clone()),
                Vec::new(),
                numbers.clone(),
            ),
            None => (PickerFocus::Model, None, None, Vec::new(), Vec::new()),
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
            account_numbers,
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
            (PickerInput::Char(_) | PickerInput::Backspace, Some(ModalState::Reasoning { .. })) => {
                return Ok(PickerInputOutcome::Consumed);
            }
            (PickerInput::Char(_) | PickerInput::Backspace, Some(ModalState::Account { .. })) => {
                return Ok(PickerInputOutcome::Consumed);
            }
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
                let vendor = model
                    .split_once('/')
                    .map_or(model.as_str(), |(vendor, _)| vendor);
                let accounts = self.eligible_keys(vendor);
                if accounts.len() > 1 {
                    let mut numbers: Vec<_> = accounts
                        .iter()
                        .filter_map(|account| account.number)
                        .collect();
                    if numbers.len() != accounts.len() {
                        self.modal = Some(ModalState::Model(selector));
                        return Err(PickerError::AccountChoiceRequired);
                    }
                    numbers.sort_unstable();
                    self.modal = Some(ModalState::Account {
                        model,
                        selector: account_selector(&numbers),
                        numbers,
                    });
                    return Ok(PickerInputOutcome::FocusChanged(PickerFocus::Account));
                }
                self.chosen_account = accounts.first().map(|account| account.id.clone());
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
            ModalState::Account {
                model,
                mut selector,
                numbers,
            } => {
                let key = selector_key(input).ok_or(PickerError::NoSelection)?;
                selector.key(key);
                if !matches!(input, PickerInput::Enter | PickerInput::SessionOnly) {
                    self.modal = Some(ModalState::Account {
                        model,
                        selector,
                        numbers,
                    });
                    return Ok(PickerInputOutcome::Consumed);
                }
                let Some(number) = selected_id(&selector).and_then(|id| id.parse::<u32>().ok())
                else {
                    self.modal = Some(ModalState::Account {
                        model,
                        selector,
                        numbers,
                    });
                    return Err(PickerError::NoSelection);
                };
                let route = model
                    .split_once('/')
                    .map_or(model.as_str(), |(route, _)| route);
                let account = self
                    .eligible_keys(route)
                    .into_iter()
                    .find(|account| account.number == Some(number));
                let Some(id) = account.map(|account| account.id.clone()) else {
                    self.modal = Some(ModalState::Account {
                        model,
                        selector,
                        numbers,
                    });
                    return Err(PickerError::AccountChoiceRequired);
                };
                let (reasoning, default) = match reasoning_configuration(&self.snapshot, &model) {
                    Ok(configuration) => configuration,
                    Err(error) => {
                        self.modal = Some(ModalState::Account {
                            model,
                            selector,
                            numbers,
                        });
                        return Err(error);
                    }
                };
                self.chosen_account = Some(id);
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
            connection: self.chosen_account.clone(),
        };
        self.close();
        Ok(PickerOutcome::ModelSwitch(request))
    }

    pub fn replace_snapshot(
        &mut self,
        snapshot: Arc<ModelCatalogueSnapshot>,
    ) -> LocalActionOutcome {
        self.base_snapshot = Arc::clone(&snapshot);
        self.snapshot = snapshot;
        self.live_accounts.clear();
        self.chosen = None;
        self.diagnostic = None;
        if self.open {
            self.modal = Some(ModalState::Model(self.model_selector()));
        }
        LocalActionOutcome::Completed
    }

    fn close(&mut self) {
        self.open = false;
        self.chosen_account = None;
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
                        reason: "reasoning settings are not available for the current model",
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

/// Stable numbers only; the connection IDs behind them never reach this list.
fn account_selector(numbers: &[u32]) -> Selector {
    Selector::with_options_unselected(
        SelectorKind::Single,
        numbers.iter().map(|number| {
            let id = number.to_string();
            SelectorOption::new(id.clone(), id)
        }),
    )
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
