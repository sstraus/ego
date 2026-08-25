//! Pure state for the model picker shared by its keyboard and slash surfaces.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKey {
    Escape,
    SessionOnly,
    Enter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    Cancelled,
    SessionInstalled,
    Persisted(UserConfigPublication),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerError {
    Closed,
    UnknownModel { slug: String },
    Unavailable {
        slug: String,
        cause: ModelAvailabilityCause,
        message: String,
    },
    UnknownReasoningSupport { slug: String },
    UnsupportedReasoning {
        slug: String,
        thinking: ThinkingLevel,
    },
    NoSelection,
    NoSession,
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

/// Catalogue-backed state independent of terminal rendering and input decoding.
pub struct ModelPicker {
    snapshot: Arc<ModelCatalogueSnapshot>,
    selection: Option<Arc<GenerationSelection>>,
    config_store: Option<Arc<dyn UserConfigStore>>,
    chosen: Option<Chosen>,
    open: bool,
}

impl ModelPicker {
    pub fn new(snapshot: Arc<ModelCatalogueSnapshot>) -> Self {
        Self {
            snapshot,
            selection: None,
            config_store: None,
            chosen: None,
            open: true,
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
            open: true,
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

    pub fn choose(&mut self, slug: &str, thinking: ThinkingLevel) -> Result<(), PickerError> {
        if !self.open {
            return Err(PickerError::Closed);
        }
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
        match &row.capabilities.reasoning_efforts {
            ReasoningEffortSupport::Unknown => {
                return Err(PickerError::UnknownReasoningSupport {
                    slug: slug.to_string(),
                });
            }
            ReasoningEffortSupport::Only(levels) if !levels.contains(&thinking) => {
                return Err(PickerError::UnsupportedReasoning {
                    slug: slug.to_string(),
                    thinking,
                });
            }
            ReasoningEffortSupport::Any | ReasoningEffortSupport::Only(_) => {}
        }
        self.chosen = Some(Chosen {
            model: slug.to_string(),
            thinking,
        });
        Ok(())
    }

    pub fn key(&mut self, key: PickerKey) -> Result<PickerOutcome, PickerError> {
        match key {
            PickerKey::Escape => {
                self.open = false;
                self.chosen = None;
                Ok(PickerOutcome::Cancelled)
            }
            PickerKey::SessionOnly => {
                let chosen = self.chosen.clone().ok_or(PickerError::NoSelection)?;
                self.install(&chosen)?;
                self.open = false;
                Ok(PickerOutcome::SessionInstalled)
            }
            PickerKey::Enter => {
                let chosen = self.chosen.clone().ok_or(PickerError::NoSelection)?;
                let store = self.config_store.as_ref().ok_or(PickerError::NoSession)?;
                let publication = store
                    .mutate(UserConfigMutation::ModelAndThinking {
                        model: chosen.model.clone(),
                        thinking: chosen.thinking,
                    })
                    .map_err(PickerError::Persistence)?;
                self.install(&chosen)?;
                self.open = false;
                Ok(PickerOutcome::Persisted(publication))
            }
        }
    }

    pub fn reopen(&mut self) {
        self.open = true;
        self.chosen = None;
    }

    pub fn replace_snapshot(&mut self, snapshot: Arc<ModelCatalogueSnapshot>) {
        self.snapshot = snapshot;
        self.chosen = None;
    }

    fn install(&self, chosen: &Chosen) -> Result<(), PickerError> {
        let selection = self.selection.as_ref().ok_or(PickerError::NoSession)?;
        selection
            .install(ModelRef(chosen.model.clone()), chosen.thinking)
            .map_err(PickerError::SessionInstall)
    }
}
