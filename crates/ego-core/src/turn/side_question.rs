//! A side question answered beside a running turn.
//!
//! The question reuses the turn's latest request, so the provider serves the
//! cached prefix. It runs on the shared budget gate and journal and books its
//! usage on the main line, but it never writes an item or an evidence blob: the
//! question and the answer stay out of the conversation history and the store.

use std::sync::{Arc, Mutex};

use super::{
    CancelToken, GenerationModelRequest, GenerationModelSource, InteractiveControl,
    ToollessGenerationOwner,
};
use crate::config::ModelRef;
use crate::event::domain::GenerationRole;
use crate::event::id::TurnId;
use crate::generator::attempt::DeliveryCertainty;
use crate::generator::types::{Conversation, GenRequest, Message, Role};
use crate::state::Usage;

/// Instruction sent after the live conversation. The tool descriptors stay in
/// the request because they are part of the cached prefix; no call is run.
const SIDE_QUESTION_PROMPT: &str = "The user asks a side question while the task above keeps \
running. Answer only this question, in plain text, from the conversation so far. Do not call \
tools: none will run, and the task is not affected. Question: ";

/// The newest request the running turn admitted, and the turn that sent it.
#[derive(Clone, Default)]
pub(super) struct LiveRequest(Arc<Mutex<Option<(GenRequest, TurnId)>>>);

impl LiveRequest {
    pub(super) fn set(&self, request: &GenRequest, turn: TurnId) {
        *self.0.lock().expect("live request lock") = Some((request.clone(), turn));
    }

    fn get(&self) -> Option<(GenRequest, TurnId)> {
        self.0.lock().expect("live request lock").clone()
    }
}

/// Why a side question gave no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideQuestionFailure {
    message: String,
    delivery: DeliveryCertainty,
}

impl SideQuestionFailure {
    fn unsent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            delivery: DeliveryCertainty::DefinitelyUnsent,
        }
    }

    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }
}

impl std::fmt::Display for SideQuestionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SideQuestionFailure {}

/// An answered side question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideAnswer {
    text: String,
    usage: Option<Usage>,
}

impl SideAnswer {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// What the provider reported for the answer, when it reported any.
    pub fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }
}

/// Answers side questions for one turn loop. Cheap to clone; usable while the
/// loop is driven, because it holds shared handles and no borrow of the loop.
#[derive(Clone)]
pub struct SideQuestionPort<'session, 'turn> {
    owner: ToollessGenerationOwner<'session, 'turn>,
    live: LiveRequest,
    source: Arc<dyn GenerationModelSource>,
    model: Option<ModelRef>,
}

impl<'session, 'turn> SideQuestionPort<'session, 'turn> {
    pub(super) fn new(
        owner: ToollessGenerationOwner<'session, 'turn>,
        live: LiveRequest,
        source: Arc<dyn GenerationModelSource>,
        model: Option<ModelRef>,
    ) -> Self {
        Self {
            owner,
            live,
            source,
            model,
        }
    }

    /// Ask one question against the running turn's context.
    pub async fn ask(
        &self,
        question: &str,
        cancel: &CancelToken,
    ) -> Result<SideAnswer, SideQuestionFailure> {
        if question.trim().is_empty() {
            return Err(SideQuestionFailure::unsent("the side question is empty"));
        }
        let Some((mut request, turn)) = self.live.get() else {
            return Err(SideQuestionFailure::unsent(
                "the turn has not sent its first request yet; ask again in a moment",
            ));
        };
        let mut selection = GenerationModelRequest::new(GenerationRole::Compaction);
        if let Some(model) = self.model.as_ref() {
            selection = selection.with_model(model.clone());
        }
        let profile = self
            .source
            .resolve(selection)
            .await
            .map_err(|error| SideQuestionFailure::unsent(error.to_string()))?;
        if profile.request_model != request.model {
            return Err(SideQuestionFailure::unsent(
                "the model changed during the turn; ask again after the next request",
            ));
        }
        let mut messages: Vec<Message> = request.messages.into();
        messages.push(Message::text(
            Role::User,
            format!("{SIDE_QUESTION_PROMPT}{question}"),
        ));
        request.messages = Conversation::new(messages)
            .map_err(|error| SideQuestionFailure::unsent(error.to_string()))?;
        // Booked as the main model's spend; the auxiliary resolution only
        // keeps the turn's own continuation owner untouched.
        let (raw, delivery, usage) = self
            .owner
            .generate_prepared(
                profile,
                request,
                GenerationRole::Main,
                turn,
                cancel,
                &mut None::<&mut InteractiveControl>,
            )
            .await
            .map_err(|error| SideQuestionFailure {
                delivery: error.delivery(),
                message: error.to_string(),
            })?;
        let text = String::from_utf8_lossy(&raw).trim().to_owned();
        if text.is_empty() {
            return Err(SideQuestionFailure {
                message: "the model returned no text".into(),
                delivery,
            });
        }
        Ok(SideAnswer { text, usage })
    }
}
