//! Read-only conversational access bound by the host to its session store.
//! Workspace effects and their permission vocabulary are not history authority.
use std::fmt;

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;

use crate::event::id::SessionId;
use crate::generator::types::ToolDescriptor;

#[derive(Debug, Clone)]
pub enum HistoryRequest {
    Search { query: String },
    Open { session: SessionId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryReadError(pub String);
impl fmt::Display for HistoryReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for HistoryReadError {}

/// The consumer asks for conversation, never arbitrary paths or tool payload.
pub trait SessionHistory: Send + Sync {
    fn read(&self, request: HistoryRequest) -> BoxFuture<'_, Result<String, HistoryReadError>>;
}

#[derive(Debug, Clone, Copy)]
pub enum HistoryTool {
    Search,
    Open,
}
impl HistoryTool {
    pub fn descriptor(self) -> ToolDescriptor {
        let (name, description, field, format) = match self {
            Self::Search => (
                "history.search",
                "Search prior conversations across workspace roots; returns titles, dates and snippets in at most 1 KiB",
                "query",
                None,
            ),
            Self::Open => (
                "history.open",
                "Read up to 8 KiB of a prior session's user and assistant conversation; excludes tool results",
                "session",
                Some("uuid"),
            ),
        };
        let mut property = serde_json::json!({ "type": "string" });
        if let Some(format) = format {
            property["format"] = format.into();
        }
        ToolDescriptor {
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "required": [field], "properties": { (field): property }
            }),
        }
    }
    pub(crate) fn parse(self, params: Value) -> Result<HistoryRequest, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Search {
            query: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Open {
            session: SessionId,
        }
        match self {
            Self::Search => {
                serde_json::from_value::<Search>(params).map(|params| HistoryRequest::Search {
                    query: params.query,
                })
            }
            Self::Open => {
                serde_json::from_value::<Open>(params).map(|params| HistoryRequest::Open {
                    session: params.session,
                })
            }
        }
        .map_err(|error| {
            format!(
                "{} was called with invalid arguments: {error}",
                self.descriptor().name
            )
        })
    }
}
