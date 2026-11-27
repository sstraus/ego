//! Shared Responses search items and client-side cutoff for routes without a count field.
use std::collections::BTreeSet;
use std::num::NonZeroU32;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ErrorKind, ReasoningState, StopReason};
use serde_json::Value;

#[derive(Debug, Default)]
pub(super) struct SearchItems {
    items: BTreeSet<String>,
    searches: BTreeSet<String>,
    budgeted: BTreeSet<String>,
}

impl SearchItems {
    pub(super) fn observed(&self) -> u64 {
        self.searches.len() as u64
    }

    /// Capture a frame, or stop before delivering an over-budget search item.
    pub(super) fn capture(
        &mut self,
        frame: &str,
        dialect: &str,
        out: &mut Vec<AiEvent>,
        limit: Option<NonZeroU32>,
    ) -> bool {
        let Ok(frame) = serde_json::from_str::<Value>(frame) else {
            return false;
        };
        let added = frame["type"] == "response.output_item.added";
        let done = frame["type"] == "response.output_item.done";
        let item = &frame["item"];
        let search = item["type"] == "web_search_call";
        let action = item["action"]["type"].as_str();
        // Only a search action spends the budget. A page open or in-page find
        // also arrives as a web_search_call and can start without an action,
        // so an action-less start is not counted; the item counts once its
        // action is known, on the added or the done frame.
        if let Some(limit) = limit
            && search
            && (added || done)
            && action == Some("search")
            && self.budgeted.insert(
                item["id"]
                    .as_str()
                    .map_or_else(|| item.to_string(), str::to_owned),
            )
            && self.budgeted.len() as u64 > u64::from(limit.get())
        {
            let cause = format!(
                "Native web search exceeded the local limit of {limit} searches. Ego stopped reading the response; the provider may already have started or charged for more searches."
            );
            out.extend([
                AiEvent::Error {
                    message: cause.clone(),
                    kind: ErrorKind::LocalLimit,
                },
                AiEvent::LocalFailure { cause },
                AiEvent::Stop(StopReason::Error),
            ]);
            return true;
        }
        if !done {
            return false;
        }
        let cited_message = item["type"] == "message"
            && item["content"].as_array().is_some_and(|parts| {
                parts.iter().any(|part| {
                    part["annotations"]
                        .as_array()
                        .is_some_and(|annotations| !annotations.is_empty())
                })
            });
        if !search && !cited_message {
            return false;
        }
        let blob = item.to_string();
        let key = item["id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| blob.clone());
        if !self.items.insert(key.clone()) {
            return false;
        }
        if search && item["action"]["type"] == "search" && self.searches.insert(key.clone()) {
            let query = item["action"]["query"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    let queries: Vec<_> = item["action"]["queries"]
                        .as_array()?
                        .iter()
                        .filter_map(Value::as_str)
                        .collect();
                    (!queries.is_empty()).then(|| queries.join("; "))
                })
                .unwrap_or_else(|| "web search".into());
            let call_id = item["id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("search-{}", self.searches.len()));
            out.push(AiEvent::WebSearch { call_id, query });
        }
        for part in item["content"].as_array().into_iter().flatten() {
            for citation in part["annotations"].as_array().into_iter().flatten() {
                if citation["type"] == "url_citation"
                    && let Some(url) = citation["url"].as_str()
                {
                    out.push(AiEvent::Citation {
                        url: url.into(),
                        title: citation["title"].as_str().unwrap_or(url).into(),
                    });
                }
            }
        }
        out.push(AiEvent::ProviderContent {
            state: ReasoningState {
                connection: None,
                dialect: dialect.into(),
                blob,
            },
            visible: cited_message,
        });
        false
    }
}

/// Keep the provider's complete object; do not reconstruct encrypted fields or annotations.
pub(super) fn replay(state: &ReasoningState, dialect: &str) -> Option<Value> {
    if state.dialect != dialect {
        return None;
    }
    let item: Value = serde_json::from_str(&state.blob).ok()?;
    match item["type"].as_str()? {
        "web_search_call" => Some(item),
        "message" if item["content"].is_array() => Some(item),
        _ => None,
    }
}
