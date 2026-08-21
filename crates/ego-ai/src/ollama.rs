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

use std::collections::BTreeSet;
use std::sync::Arc;

use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::types::ThinkingLevel;
use futures::StreamExt;
use serde::Deserialize;

use crate::catalogue::{ResolveError, ResolvedModel, control_plane, is_locally_served, unmetered};
use crate::http::{Http, HttpRequest, HttpResponse, Method};
use crate::provider::ProviderRow;

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
/// Takes no currency. A locally served model is not metered in one — it has
/// no per-token price to express in any money — and the zero the budget gate
/// reserves for it is minted in the gate's own limit currency, which is the
/// only currency that zero could be compared against.
pub async fn resolve(
    row: &ProviderRow,
    model: &str,
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
        metering: unmetered(),
        // Nobody publishes a price for weights the daemon serves itself, and
        // `Unmetered` already says the charge is zero. A quote here would be
        // a price list for something that has no prices.
        rates: None,
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
/// request. `max_output_tokens` is `None` for the same reason: the daemon
/// states no output ceiling, and the gate refuses with
/// `UnstatedCeiling { Output }` if the request does not state one either.
///
/// The second half of that sentence used to read "so the request must cap
/// itself", and that was measured and found false. **The request cannot cap
/// itself on this row.** Ollama's OpenAI-compatible endpoint honours
/// `max_tokens` and ignores `max_completion_tokens`, which is the field
/// `wire::openai_completions` sends — correctly, since `max_tokens` is
/// deprecated at OpenAI and a shared wire format is not where a provider's
/// name belongs. Recorded from a live 0.32.14 daemon in
/// `crates/ego-ai/tests/ollama_ceiling.rs`, request and response both.
///
/// So the gate's refusal is currently doing the only real work: it forces a
/// ceiling to be named, and the daemon then discards it. Closing that is a
/// mapping decision one layer up — this row is further from the
/// openai-completions family than a shared endpoint path suggests — and it is
/// deliberately not made here.
fn capabilities_from(tag: &Tag, served_context: u64) -> ModelCapabilities {
    let thinks = tag.capabilities.iter().any(|cap| cap == "thinking");
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
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
        // Stated, and stating it is what makes the sentence above enforceable:
        // `SameProviderOnly` constrains whose state is accepted, so without a
        // tag on this side the check cannot run and foreign state passes. The
        // daemon mints no reasoning state of its own — it streams thinking as
        // text, never as a replayable blob — so nothing will ever carry this
        // tag, and every carried state is therefore foreign and refused by
        // name. That is the intended answer arrived at by comparison rather
        // than by an unknown.
        reasoning_state_dialect: Some("ollama".to_string()),
        // The five levels Ollama *honours*, which is not the seven it
        // *accepts*. Read from `openai/openai.go::thinkFromReasoningEffort`
        // at tag v0.32.14 — the same version `provider.rs` measured:
        //
        //     case "minimal":                  return "low"
        //     case "low","medium","high","max": return effort
        //     case "xhigh","ultra":            return "max"
        //
        // with a source comment electing to "clamp those to the nearest
        // Ollama tier rather than rejecting the request". So `minimal` and
        // `xhigh` come back one tier away from what the caller asked for,
        // silently and with a 200.
        //
        // This field was `Any` until the wire survey caught it. The earlier
        // measurement was not wrong, it was narrower than it read: it proved
        // every level is *accepted*, and accepted was taken for honoured.
        // That is the mistranslation `docs/08-codex-critique.md` I5 calls
        // worse than a refusal, and `ReasoningEffortSupport`'s own preamble
        // says the parameter is not advisory. Naming the five refuses the
        // other two at `request_gaps`, before the request is built, telling
        // the caller which levels would have worked.
        reasoning_efforts: ReasoningEffortSupport::Only(BTreeSet::from([
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Max,
        ])),
        // Dropping the connection stops the runner at a token boundary.
        cancel: CancelSemantics::Cooperative,
        // Ollama's prompt cache does not survive the model being unloaded,
        // which `keep_alive` does on a timer measured in minutes.
        cache: CacheRetention::Short,
        transport: Transport::Sse,
    }
}
