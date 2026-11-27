//! A short recap of the session for a user who comes back after an absence.
//!
//! Asked before the next prompt joins the history, from the source context
//! compaction reads ([`TurnLoop::instruction_request`]). It runs on the shared
//! budget gate and journal and books its usage on the main line, but it writes
//! no item and no evidence blob: the recap stays out of the history.

use super::{CancelToken, GenerationModelRequest, InteractiveControl, TurnLoop};
use crate::event::domain::GenerationRole;
use crate::event::id::TurnId;
use crate::generator::types::{Conversation, Message, Role};
use crate::state::Usage;

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

impl<'session: 'turn, 'turn> TurnLoop<'session, 'turn> {
    /// Ask the model for a recap of the history so far.
    pub async fn recap(&mut self, cancel: &CancelToken) -> Result<Recap, RecapFailure> {
        if super::batch::pending_outer_call_id(&self.state()).is_some() {
            return Err(failed(
                "answer the pending MCP batch question before requesting a recap",
            ));
        }
        self.settle_suspended_batches(None).map_err(failed)?;
        let (history, _repairs) = self.rebuild_history().map_err(failed)?;
        let history = self.with_system(history);
        let mut messages: Vec<Message> = history.clone().into();
        messages.extend(self.durable_skill_messages().map_err(failed)?);
        messages.push(Message::text(
            Role::User,
            crate::model_prompts::RECAP_PROMPT,
        ));
        let messages = Conversation::new(messages).map_err(failed)?;
        let mut selection = GenerationModelRequest::new(GenerationRole::Compaction);
        if let Some(model) = self.main_model_identity.as_ref() {
            selection = selection.with_model(model.clone());
        }
        let profile = self.source.resolve(selection).await.map_err(failed)?;
        // The interactive driver builds a fresh loop per turn. Capture the
        // permitted tools for cold history, without default hosted search.
        // instruction_request removes the search descriptor on both paths;
        // other descriptors remain available for replayed tool history.
        let tools = match &self.last_request {
            Some(request) => request.tools.clone(),
            None => {
                let ceiling = self.effect_ceiling();
                let mut catalogue = self
                    .tools
                    .capture_with_effect_ceiling(ceiling.as_deref())
                    .map_err(failed)?;
                catalogue.freeze_skills(&mut self.prompt_snapshot.skills);
                catalogue
                    .for_provider(
                        profile.generator.supports_web_search(),
                        false,
                        profile.capabilities.supports_parallel_tool_calls,
                    )
                    .descriptors()
            }
        };
        let request = self.instruction_request(
            &profile,
            &history,
            messages,
            crate::model_prompts::RECAP_PROMPT,
            tools,
        );
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
