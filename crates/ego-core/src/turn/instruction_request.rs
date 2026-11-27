//! One instruction asked after the current history, as a cache-friendly
//! request. Compaction and the away recap share its source context. Removing
//! hosted search changes a search-enabled main request's tool prefix; provider
//! cache reuse is not guaranteed.

use super::{GenerationProfile, TurnLoop};
use crate::generator::types::{Conversation, GenRequest, Message, Role, ToolDescriptor};

impl TurnLoop<'_, '_> {
    /// The request that asks `instruction` after `history`.
    ///
    /// When `history` extends the previous request, the previous request is
    /// the prefix and only the new blocks and the instruction follow it, so the
    /// provider can read its cache. Otherwise `messages` (which already ends
    /// with the instruction) is sent with a cache marker before the
    /// instruction, and `tools` as the tool set.
    /// Hosted search and its descriptor are removed from either path; this may
    /// change the tool prefix and reduce cache reuse.
    ///
    /// Both histories lose the provider state `profile` does not accept
    /// first, the same removal a turn applies: a session resumed on another
    /// provider still compacts and recaps, the prefix still matches the
    /// previous request, and the cache marker counts the messages sent.
    pub(super) fn instruction_request(
        &self,
        profile: &GenerationProfile,
        history: &Conversation,
        messages: Conversation,
        instruction: &str,
        tools: Vec<ToolDescriptor>,
    ) -> GenRequest {
        // Ancillary requests use the same captured candidate policy as main
        // generations, before prefix comparison and stable-message accounting.
        let history = &crate::generator::capabilities::project_tool_images(
            super::drop_unaccepted_state(history.clone(), &profile.capabilities).0,
            &profile.capabilities,
        );
        let messages = crate::generator::capabilities::project_tool_images(
            super::drop_unaccepted_state(messages, &profile.capabilities).0,
            &profile.capabilities,
        );
        let live = self.last_request.as_ref().and_then(|previous| {
            // History coalesces adjacent messages, so compare content blocks,
            // not message counts. Include the source's final response/results.
            let blocks = |conversation: &Conversation| {
                conversation
                    .messages()
                    .iter()
                    .filter(|message| message.role != Role::System)
                    .flat_map(|message| {
                        message
                            .content
                            .iter()
                            .map(|block| (message.role, block.clone()))
                    })
                    .collect::<Vec<_>>()
            };
            let old = blocks(&previous.messages);
            let current = blocks(history);
            if !current.starts_with(&old) {
                return None;
            }
            let mut request = previous.clone();
            let mut extended: Vec<Message> = request.messages.into();
            extended.extend(
                current
                    .into_iter()
                    .skip(old.len())
                    .map(|(role, block)| Message {
                        role,
                        content: vec![block],
                    }),
            );
            extended.push(Message::text(Role::User, instruction));
            request.messages = Conversation::new(extended).ok()?;
            Some(request)
        });
        let mut request = live.unwrap_or_else(|| {
            let stable_messages = messages.messages().len().saturating_sub(1);
            GenRequest {
                web_search: None,
                cache: Some(crate::generator::types::PromptCache {
                    verifier: None,
                    session_id: self.session_id,
                    lineage_root: self.lineage_root,
                    retention: self.config.cache,
                    stable_messages,
                }),
                model: profile.request_model.clone(),
                messages,
                tools,
                thinking: profile.thinking,
                max_output_tokens: profile.output_ceiling(),
                parallel_tool_calls: profile.capabilities.supports_parallel_tool_calls,
            }
        });
        without_hosted_search(&mut request);
        request
    }
}

/// Ancillary generations may read the main context, but cannot run hosted
/// search or advertise its descriptor as an ordinary callable function.
pub(super) fn without_hosted_search(request: &mut GenRequest) {
    request.web_search = None;
    request.tools.retain(|tool| tool.name != "web_search");
}
