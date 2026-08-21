//! Resolving a local model by asking the daemon, instead of writing down what
//! it probably says.
//!
//! See `docs/decisions/2026-08-21-what-a-model-resolves-to.md` §3 and §4.
//!
//! ## Why the context window is measured and not read
//!
//! Ollama reports two different context lengths and only one of them is true
//! of the request you are about to send. Measured on 0.32.14, same model:
//!
//! - `POST /api/show` → `model_info."<family>.context_length"` = **131072**.
//!   That is what the *architecture* supports.
//! - `GET /api/ps` → `context_length` = **32768**. That is what the daemon
//!   will actually serve, and it depends on `OLLAMA_CONTEXT_LENGTH` /
//!   `num_ctx` — a server-side setting no API exposes and no catalogue,
//!   generated or otherwise, could ever contain.
//!
//! Taking the first would tell the budget gate, the `context.used_tokens`
//! occupancy of `CLI.md` §4.1, and eventually M3's compaction trigger that
//! there is four times the room there is. So the served value is observed.
//!
//! `/api/ps` answers only for models loaded right now, so a cold model is
//! loaded first — with the daemon's own load-only request, not with a token
//! of generation. The distinction is the whole reason this is honest:
//! `POST /api/chat {"messages":[]}` answers `done_reason: "load"` with empty
//! content and no usage, so it is a control-plane operation with nothing to
//! hide from the log. A one-token generation would produce output and usage
//! that would then have to be concealed.
//!
//! Verified end to end on 0.32.14: load → `/api/ps` reports 32768 → a
//! generation through `/v1/chat/completions` (the endpoint `WireGenerator`
//! uses) succeeds and `/api/ps` still reports 32768. Same runner, same
//! context, no reload.

use std::sync::Arc;

use ego_core::cost::CurrencyCode;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, Transport,
};
use futures::StreamExt;
use serde::Deserialize;

use crate::catalogue::{ResolveError, ResolvedModel, control_plane, is_locally_served, unmetered};
use crate::http::{Http, HttpRequest, HttpResponse, Method};
use crate::provider::ProviderRow;

/// The capability schema version a resolved local model is stamped with.
const SCHEMA_VERSION: u32 = 1;

/// One entry of `GET /api/tags`.
#[derive(Debug, Deserialize)]
struct Tag {
    name: String,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Tags {
    #[serde(default)]
    models: Vec<Tag>,
}

/// One entry of `GET /api/ps`. `context_length` is the served ceiling.
#[derive(Debug, Deserialize)]
struct Running {
    name: String,
    context_length: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Ps {
    #[serde(default)]
    models: Vec<Running>,
}

/// Resolve a local model against the daemon `row` points at.
///
/// `currency` is the session's settlement currency: the zero quote asserts
/// "this provider charges nothing", and a claim about money still has to be
/// expressed in the money the session settles in.
pub async fn resolve(
    row: &ProviderRow,
    model: &str,
    currency: CurrencyCode,
    http: &Arc<dyn Http>,
) -> Result<ResolvedModel, ResolveError> {
    // Before anything is asked of the daemon. A `:cloud` identifier is
    // relayed upstream on someone's account, so it is neither local nor free,
    // and no amount of asking a loopback address will reveal that.
    if !is_locally_served(model) {
        return Err(ResolveError::NotLocallyServed {
            model: model.to_string(),
        });
    }

    let tag = installed(row, model, http).await?;
    let context = served_context(row, model, http).await?;

    Ok(ResolvedModel {
        model: model.to_string(),
        row: row.clone(),
        capabilities: capabilities_from(&tag, context),
        pricing: unmetered(currency),
    })
}

/// The daemon's own entry for `model`, or the list it does serve.
async fn installed(
    row: &ProviderRow,
    model: &str,
    http: &Arc<dyn Http>,
) -> Result<Tag, ResolveError> {
    let url = control_plane(row, "/api/tags")?;
    let body = fetch(
        http,
        HttpRequest {
            url: url.clone(),
            method: Method::Get,
            headers: vec![("accept".to_string(), "application/json".to_string())],
            body: None,
        },
    )
    .await
    .map_err(|message| ResolveError::DaemonUnreachable {
        endpoint: url.to_string(),
        message,
    })?;

    let tags: Tags =
        serde_json::from_slice(&body).map_err(|err| ResolveError::DaemonUnreachable {
            endpoint: url.to_string(),
            message: format!("its model list did not parse: {err}"),
        })?;

    tags.models
        .iter()
        .find(|tag| tag.name == model)
        .map(|tag| Tag {
            name: tag.name.clone(),
            capabilities: tag.capabilities.clone(),
        })
        .ok_or_else(|| ResolveError::NotInstalled {
            model: model.to_string(),
            installed: tags.models.iter().map(|tag| tag.name.clone()).collect(),
        })
}

/// The context the daemon will actually serve for `model`, loading it if it
/// is not already running.
///
/// The load is not retried if the model still fails to appear afterwards: a
/// daemon that reports a successful load and then does not list the model is
/// in a state this code should report, not paper over.
async fn served_context(
    row: &ProviderRow,
    model: &str,
    http: &Arc<dyn Http>,
) -> Result<u64, ResolveError> {
    if let Some(context) = running_context(row, model, http).await? {
        return Ok(context);
    }
    load(row, model, http).await?;
    running_context(row, model, http)
        .await?
        .ok_or(ResolveError::Undiscoverable {
            model: model.to_string(),
            fact: "served context window, even after it was loaded",
        })
}

/// `GET /api/ps`, filtered to `model`. `None` means it is not loaded.
async fn running_context(
    row: &ProviderRow,
    model: &str,
    http: &Arc<dyn Http>,
) -> Result<Option<u64>, ResolveError> {
    let url = control_plane(row, "/api/ps")?;
    let body = fetch(
        http,
        HttpRequest {
            url: url.clone(),
            method: Method::Get,
            headers: vec![("accept".to_string(), "application/json".to_string())],
            body: None,
        },
    )
    .await
    .map_err(|message| ResolveError::DaemonUnreachable {
        endpoint: url.to_string(),
        message,
    })?;

    let ps: Ps = serde_json::from_slice(&body).map_err(|err| ResolveError::DaemonUnreachable {
        endpoint: url.to_string(),
        message: format!("its running-model list did not parse: {err}"),
    })?;

    Ok(ps
        .models
        .into_iter()
        .find(|running| running.name == model)
        .and_then(|running| running.context_length))
}

/// The daemon's load-only request: `messages: []`, which answers
/// `done_reason: "load"` with no content and no usage.
async fn load(row: &ProviderRow, model: &str, http: &Arc<dyn Http>) -> Result<(), ResolveError> {
    let url = control_plane(row, "/api/chat")?;
    let body = serde_json::json!({ "model": model, "messages": [] });
    fetch(
        http,
        HttpRequest {
            url: url.clone(),
            method: Method::Post,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Some(serde_json::to_vec(&body).expect("a two-key object of strings")),
        },
    )
    .await
    .map(|_| ())
    .map_err(|message| ResolveError::DaemonUnreachable {
        endpoint: url.to_string(),
        message,
    })
}

/// One control-plane exchange, buffered.
///
/// Buffering is right here and wrong for a generation: these bodies are a
/// model list and a status object, and the whole point of the call is to have
/// all of it before deciding anything.
async fn fetch(http: &Arc<dyn Http>, req: HttpRequest) -> Result<Vec<u8>, String> {
    let HttpResponse {
        status,
        headers: _,
        mut body,
    } = http.execute(req).await.map_err(|err| format!("{err:?}"))?;

    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        bytes.extend_from_slice(&chunk.map_err(|err| format!("{err:?}"))?);
    }
    if !(200..300).contains(&status) {
        return Err(format!(
            "it answered {status}: {}",
            String::from_utf8_lossy(&bytes)
        ));
    }
    Ok(bytes)
}

/// What the daemon stated, and nothing more.
///
/// `/api/tags` reports `["completion","tools","thinking"]` and says nothing
/// about parallel tool calls, so `supports_parallel_tool_calls` is `false`:
/// the value that only forecloses a fallback, never the one that breaks a
/// request. `max_output_tokens` is `None` for the same reason — the daemon
/// states no output ceiling, so the request must cap itself, and the gate
/// refuses with `UnstatedCeiling { Output }` if neither does.
fn capabilities_from(tag: &Tag, served_context: u64) -> ModelCapabilities {
    let thinks = tag.capabilities.iter().any(|cap| cap == "thinking");
    ModelCapabilities {
        schema_version: SCHEMA_VERSION,
        max_context_tokens: Some(served_context),
        max_output_tokens: None,
        max_tools: None,
        supports_parallel_tool_calls: false,
        // A local daemon holds no reasoning state across requests and has no
        // provider-side conversation to carry it in, so state produced
        // elsewhere cannot be replayed into it. A model that cannot think at
        // all is the same answer for a plainer reason.
        reasoning_carryover: if thinks {
            ReasoningCarryover::SameProviderOnly
        } else {
            ReasoningCarryover::Unsupported
        },
        // Dropping the connection stops the runner at a token boundary.
        cancel: CancelSemantics::Cooperative,
        // Ollama's prompt cache does not survive the model being unloaded,
        // which `keep_alive` does on a timer measured in minutes.
        cache: CacheRetention::Short,
        transport: Transport::Sse,
    }
}
