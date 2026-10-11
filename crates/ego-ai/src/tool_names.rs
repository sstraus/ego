//! Request-local provider aliases; catalogue and journal names stay unchanged.

use std::collections::{BTreeMap, BTreeSet};

use ego_core::digest::sha256_hex;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ContentBlock, Conversation, ErrorKind, GenRequest, StopReason};

const PREFIX: &str = "__ego_";
const MAX_BYTES: usize = 64;

pub(crate) struct ToolNames {
    encoded: BTreeMap<String, String>,
    originals: BTreeMap<String, String>,
}

impl ToolNames {
    pub(crate) fn new(request: &GenRequest) -> Self {
        let names: BTreeSet<&str> = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .chain(
                request
                    .messages
                    .messages()
                    .iter()
                    .flat_map(|message| &message.content)
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
                        _ => None,
                    }),
            )
            .collect();
        // Reserve valid originals first. Reserved-prefix originals must also be
        // encoded, so none can impersonate the alias of a different tool.
        let mut encoded = BTreeMap::new();
        let mut originals = BTreeMap::new();
        for name in names.iter().copied().filter(|name| portable(name)) {
            encoded.insert(name.to_owned(), name.to_owned());
            originals.insert(name.to_owned(), name.to_owned());
        }
        for name in names.into_iter().filter(|name| !portable(name)) {
            let hash = sha256_hex(name.as_bytes());
            let mut number = 0_u64;
            let alias = loop {
                let suffix = if number == 0 {
                    String::new()
                } else {
                    format!("_{number}")
                };
                let alias = format!(
                    "{PREFIX}{}{suffix}",
                    &hash[..MAX_BYTES - PREFIX.len() - suffix.len()]
                );
                if !originals.contains_key(&alias) {
                    break alias;
                }
                number = number
                    .checked_add(1)
                    .expect("a request cannot contain 2^64 distinct tools");
            };
            encoded.insert(name.to_owned(), alias.clone());
            originals.insert(alias, name.to_owned());
        }
        Self { encoded, originals }
    }

    pub(crate) fn encode(&self, request: &GenRequest) -> GenRequest {
        let mut encoded = request.clone();
        for tool in &mut encoded.tools {
            tool.name = self.encoded[&tool.name].clone();
        }
        let mut messages: Vec<_> = encoded.messages.into();
        for block in messages.iter_mut().flat_map(|message| &mut message.content) {
            if let ContentBlock::ToolUse { name, .. } = block {
                *name = self.encoded[name.as_str()].clone();
            }
        }
        encoded.messages = Conversation::new(messages)
            .expect("renaming tools preserves conversation call/result invariants");
        encoded
    }

    pub(crate) fn restore(&self, mut event: AiEvent) -> Vec<AiEvent> {
        if let AiEvent::ToolCall { name, .. } = &mut event {
            let Some(original) = self.originals.get(name.as_str()) else {
                // End the generation as well: a later provider ToolUse stop
                // must not turn this rejected call into a successful attempt.
                return vec![
                    AiEvent::Error {
                        message: format!("provider returned undeclared tool name '{name}'"),
                        kind: ErrorKind::Unclassified,
                    },
                    AiEvent::Stop(StopReason::Error),
                ];
            };
            name.clone_from(original);
        }
        vec![event]
    }
}

fn portable(name: &str) -> bool {
    !name.starts_with(PREFIX)
        && !name.is_empty()
        && name.len() <= MAX_BYTES
        && name
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
