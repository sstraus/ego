//! A short recap of the session for a user who comes back after an absence.
//!
//! Asked before the next prompt joins the history, from the same cached prefix
//! compaction reads ([`TurnLoop::instruction_request`]). It runs on the shared
//! budget gate and journal and books its usage on the main line, but it writes
//! no item and no evidence blob: the recap stays out of the history.

use super::{CancelToken, GenerationModelRequest, InteractiveControl, TurnLoop};
use crate::event::domain::GenerationRole;
use crate::event::id::TurnId;
use crate::generator::types::{Conversation, Message, Role};
use crate::state::Usage;

/// Instruction sent after the history. The tools stay in the request because
/// they are part of the cached prefix; no call is run.
const RECAP_PROMPT: &str = "The user is back after a break. In at most five short lines of plain \
text, recap this session for them: the goal, what is done, what is open, and the next step. Do \
not call tools: none will run.";

/// A recap and what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recap {
    text: String,
    usage: Option<Usage>,
}

impl Recap {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// What the provider reported for the recap, when it reported any.
    pub fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }
}

/// Why no recap was shown. The prompt still runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecapFailure(String);

impl std::fmt::Display for RecapFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RecapFailure {}

fn failed(error: impl std::fmt::Display) -> RecapFailure {
    RecapFailure(error.to_string())
}

impl TurnLoop<'_, '_> {
    /// Ask the model for a recap of the history so far.
    pub async fn recap(&mut self, cancel: &CancelToken) -> Result<Recap, RecapFailure> {
        let (history, _repairs) = self.rebuild_history().map_err(failed)?;
        let history = self.with_system(history);
        let mut messages: Vec<Message> = history.clone().into();
        messages.extend(self.durable_skill_messages().map_err(failed)?);
        messages.push(Message::text(Role::User, RECAP_PROMPT));
        let messages = Conversation::new(messages).map_err(failed)?;
        let mut selection = GenerationModelRequest::new(GenerationRole::Compaction);
        if let Some(model) = self.main_model_identity.as_ref() {
            selection = selection.with_model(model.clone());
        }
        let profile = self.source.resolve(selection).await.map_err(failed)?;
        // The interactive driver builds a fresh loop per turn, so the tool
        // list is the one the next main request will send: the same capture
        // and provider projection, which keeps the cached prefix and makes
        // replayed tool calls valid.
        let tools = match &self.last_request {
            Some(request) => request.tools.clone(),
            None => {
                let ceiling = self.effect_ceiling();
                self.tools
                    .capture_with_effect_ceiling(ceiling.as_deref())
                    .map_err(failed)?
                    .for_provider(profile.generator.supports_web_search())
                    .descriptors()
            }
        };
        let request = self.instruction_request(&profile, &history, messages, RECAP_PROMPT, tools);
        // Booked as the main model's spend, like a side question.
        let (raw, _delivery, usage) = self
            .toolless_generation_owner()
            .generate_prepared(
                profile,
                request,
                GenerationRole::Main,
                TurnId::new(),
                cancel,
                &mut None::<&mut InteractiveControl>,
            )
            .await
            .map_err(failed)?;
        let text = String::from_utf8_lossy(&raw).trim().to_owned();
        if text.is_empty() {
            return Err(failed("the model returned no text"));
        }
        Ok(Recap { text, usage })
    }
}
