//! DeepSeek's Chat Completions endpoint, as its own mapping.
//!
//! It is a member of the [`openai_completions`] family on the response side
//! and differs on the request side in four published facts
//! (<https://api-docs.deepseek.com/api/create-chat-completion> and
//! <https://api-docs.deepseek.com/guides/thinking_mode>, read 2026-11-14):
//!
//! - The output ceiling is `max_tokens`. `max_completion_tokens` is not in
//!   the schema, so its effect is unknown and this mapping does not send it.
//! - Thinking is switched by `thinking: {"type": "enabled" | "disabled"}`,
//!   which takes precedence over `reasoning_effort`; thinking is on by
//!   default.
//! - The native efforts are `low`, `high` and `max`. The server maps
//!   `minimal` to `low` and `medium`/`xhigh` to `high` without saying so, so
//!   this mapping refuses those three levels before any request instead of
//!   letting the server pick another level.
//! - With `tools` in the request, the `reasoning_content` of every earlier
//!   assistant turn must be sent back, or the server answers `400`.
//!
//! The response side differs in one fact: the cache split comes from the
//! required `prompt_cache_hit_tokens` and `prompt_cache_miss_tokens`, not from
//! the optional `cached_tokens` alias (see [`map_cache_usage`]).
//!
//! Hosted search follows <https://api-docs.deepseek.com/guides/web_tools>
//! (snapshot of 2026-11-14): `tools: [{"type": "deepseek_web_search"}]`; with
//! thinking off the model searches and cites, with thinking on it runs deep
//! thinking search. The guide documents only requests whose tools are these
//! server tools, so a search request that also declares a function tool is
//! refused, not stripped. The guide names no per-model capability field, so
//! the mapping opts in to explicit selection only and never to default search,
//! and the server decides which model serves it. The guide has no use cap,
//! so [`HostedSearch::max_requests`](ego_core::generator::types::HostedSearch)
//! is not expressible; deep thinking search makes up to about 20 calls. Since
//! 2026-11-18 the pricing page bills these calls as model tokens, not per
//! search, so `web_search_requests` stays a measured zero.
//!
//! `parallel_tool_calls` is not in the schema either and is omitted. The
//! request body is [`openai_completions::common_request`] plus these fields;
//! the response side is the family's decoder, replaying the reasoning of
//! [`DEEPSEEK_REASONING_DIALECT`].

use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{GenRequest, ThinkingLevel};
use futures::stream::BoxStream;
use serde::Serialize;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::openai_completions::{self, CommonRequest};
use super::provider_cost::ProviderCost;
use super::{RequestRefused, RequestTarget, WireFormat};

/// The reasoning-state dialect this mapping mints and replays: the plain
/// `reasoning_content` text of a finished assistant turn.
pub const DEEPSEEK_REASONING_DIALECT: &str = "deepseek-chat";

/// The path this mapping joins onto a row's base URL. DeepSeek serves it
/// without a `/v1` segment, which is the row's base and not this path.
pub const PATH: &str = "chat/completions";

/// The `deepseek` mapping, as a value a profile can name.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeepSeek;

/// The declaration ego offers for provider-executed search. In a search
/// request it becomes the server tool, never a function the client runs.
const HOSTED_SEARCH_DECLARATION: &str = "web_search";

impl WireFormat for DeepSeek {
    fn supports_web_search(&self) -> bool {
        true
    }

    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        build_request(target, req)
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        openai_completions::decode_with_usage_mapping(
            response,
            cost,
            DEEPSEEK_REASONING_DIALECT,
            map_cache_usage,
        )
    }
}

/// The cache split from DeepSeek's required counters.
///
/// The reference makes `prompt_cache_hit_tokens` and
/// `prompt_cache_miss_tokens` required and states `prompt_tokens = hit +
/// miss`; `cached_tokens` is optional and "the same as" the hit count. The
/// schema nests the pair under `prompt_tokens_details`, the examples put it
/// at the top level, so both places are read. A split that does not add up,
/// a single counter, or an alias that disagrees with the hit count discards
/// the report: a repaired number cannot be told from a real one.
fn map_cache_usage(
    raw: &serde_json::value::RawValue,
    mut usage: ego_core::state::Usage,
) -> Option<ego_core::state::Usage> {
    let wire: serde_json::Value = serde_json::from_str(raw.get()).ok()?;
    let counter = |name: &str| {
        wire[name]
            .as_u64()
            .or_else(|| wire["prompt_tokens_details"][name].as_u64())
    };
    let alias = wire["prompt_tokens_details"]["cached_tokens"].as_u64();
    match (
        counter("prompt_cache_hit_tokens"),
        counter("prompt_cache_miss_tokens"),
    ) {
        (Some(hit), Some(miss))
            if hit.checked_add(miss)? == wire["prompt_tokens"].as_u64()?
                && alias.is_none_or(|cached| cached == hit) =>
        {
            usage.cache_read_tokens = hit;
            usage.uncached_input_tokens = miss;
            usage.cache_write_tokens = 0;
            Some(usage)
        }
        (None, None) if alias.is_some() => Some(usage),
        _ => None,
    }
}

#[derive(Debug, Serialize)]
struct Thinking {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    #[serde(flatten)]
    common: CommonRequest<'a>,
    thinking: Thinking,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    /// The server tools of a search request. Empty otherwise, and then the
    /// function tools of `common` are the request's `tools`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ServerTool>,
}

#[derive(Debug, Serialize)]
struct ServerTool {
    #[serde(rename = "type")]
    kind: &'static str,
}

/// The search-only request DeepSeek documents, or the refusal for a request
/// that would also need a function tool.
fn search_only(req: &GenRequest) -> Result<GenRequest, RequestRefused> {
    let functions: Vec<&str> = req
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .filter(|name| *name != HOSTED_SEARCH_DECLARATION)
        .collect();
    if !functions.is_empty() {
        return Err(RequestRefused::new(format!(
            "DeepSeek documents hosted search only in a request without function tools, \
             and this request also declares {}; select `--tools web_search` alone to \
             search, or deselect `web_search` to keep the other tools",
            functions.join(", ")
        )));
    }
    Ok(GenRequest {
        tools: Vec::new(),
        ..req.clone()
    })
}

/// The thinking switch and the effort DeepSeek serves for `level`, or the
/// refusal for a level it would silently replace.
fn thinking(level: ThinkingLevel) -> Result<(&'static str, Option<&'static str>), RequestRefused> {
    match level {
        // `disabled` and no effort: the switch alone selects the mode, and an
        // effort beside it would be a second statement the switch overrides.
        ThinkingLevel::Off => Ok(("disabled", None)),
        ThinkingLevel::Low => Ok(("enabled", Some("low"))),
        ThinkingLevel::High => Ok(("enabled", Some("high"))),
        ThinkingLevel::Max => Ok(("enabled", Some("max"))),
        ThinkingLevel::Minimal | ThinkingLevel::Medium | ThinkingLevel::XHigh => {
            Err(RequestRefused::new(format!(
                "DeepSeek has no `{}` reasoning effort; it serves off, low, high and max",
                level.as_str()
            )))
        }
    }
}

/// The exact JSON bytes this mapping would send for `req`.
pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, RequestRefused> {
    if super::request_has_audio(req) {
        return Err(RequestRefused::new(
            "this wire contract does not support audio input",
        ));
    }
    let (switch, effort) = thinking(req.thinking)?;
    let searching = req.web_search.map(|_| search_only(req)).transpose()?;
    let common_req = searching.as_ref().unwrap_or(req);
    let wire = WireRequest {
        common: openai_completions::common_request(common_req, Some(DEEPSEEK_REASONING_DIALECT))?
            .without_parallel_tool_calls()
            .with_reasoning_effort(effort),
        thinking: Thinking { kind: switch },
        max_tokens: req.max_output_tokens,
        tools: searching
            .iter()
            .map(|_| ServerTool {
                kind: "deepseek_web_search",
            })
            .collect(),
    };
    Ok(serde_json::to_vec(&wire).expect("WireRequest has no non-string map keys and no NaN"))
}

/// A complete HTTP request for `req` against `target`.
pub fn build_request(
    target: &RequestTarget<'_>,
    req: &GenRequest,
) -> Result<HttpRequest, RequestRefused> {
    Ok(target.post_json(PATH, request_body(req)?))
}
