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
    CacheRetention, CancelSemantics, ImageInputSupport, ImageLimits, ModelCapabilities,
    ReasoningCarryover, ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::{ImageMediaType, ThinkingLevel};
use serde::Deserialize;

use crate::catalogue::{
    ModelDiscoveryEntry, ModelDiscoveryFailure, ModelDiscoveryReport, ModelDiscoverySource,
    ModelFactProvenance, ModelSlug, ResolveError, ResolvedModel, control_plane, is_locally_served,
    unmetered,
};
use crate::http::{Http, HttpRequest, Method, fetch, public_header};
use crate::provider::ProviderRow;
use ego_core::public::MeteringKind;

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
    if embedding_only(&tag) {
        return Err(ResolveError::NotChatCapable {
            model: model.to_string(),
        });
    }
    let context = served_context(row, model, http).await?;

    Ok(ResolvedModel {
        model: model.to_string(),
        row: row.clone(),
        wire: &crate::wire::ollama::Ollama,
        capabilities: capabilities_from(&tag, Some(context)),
        metering: unmetered(),
        // Nobody publishes a price for weights the daemon serves itself, and
        // `Unmetered` already says the charge is zero. A quote here would be
        // a price list for something that has no prices.
        rates: None,
    })
}

/// Enumerate installed local models without loading any of them.
///
/// `/api/tags` does not state served context or output ceilings, so both stay
/// unknown. Transport and document failures are represented in the report so
/// a caller can merge them with the last coherent cache slice.
pub async fn enumerate(
    row: &ProviderRow,
    http: &Arc<dyn Http>,
) -> Result<ModelDiscoveryReport, String> {
    let url = control_plane(row, "/api/tags").map_err(|error| error.to_string())?;
    let body = match fetch(
        http,
        &row.http_profile,
        HttpRequest {
            url: url.clone(),
            method: Method::Get,
            headers: vec![public_header("accept", b"application/json".to_vec())],
            body: None,
        },
    )
    .await
    {
        Ok(body) => body,
        Err(message) => {
            return ModelDiscoveryReport::try_new(
                ModelDiscoverySource::Discovered {
                    provider: row.id().to_owned(),
                },
                Vec::new(),
                vec![ModelDiscoveryFailure::source(
                    "source_unreachable",
                    format!("{url} did not answer: {message}"),
                )],
                false,
            );
        }
    };

    let document: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(document) => document,
        Err(error) => {
            return ModelDiscoveryReport::try_new(
                ModelDiscoverySource::Discovered {
                    provider: row.id().to_owned(),
                },
                Vec::new(),
                vec![ModelDiscoveryFailure::source(
                    "source_malformed",
                    format!("{url} returned an invalid model list: {error}"),
                )],
                false,
            );
        }
    };
    let Some(items) = document.get("models").and_then(serde_json::Value::as_array) else {
        return ModelDiscoveryReport::try_new(
            ModelDiscoverySource::Discovered {
                provider: row.id().to_owned(),
            },
            Vec::new(),
            vec![ModelDiscoveryFailure::source(
                "source_malformed",
                format!("{url} returned no model array"),
            )],
            false,
        );
    };

    let mut models = Vec::new();
    let mut failures = Vec::new();
    let mut complete = true;
    for item in items {
        let stated_name = item
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let tag = match serde_json::from_value::<Tag>(item.clone()) {
            Ok(tag) => tag,
            Err(error) => {
                let message = format!("invalid model facts: {error}");
                if let Some(name) = stated_name {
                    failures.push(ModelDiscoveryFailure::named(
                        name,
                        "source_malformed",
                        message,
                    ));
                } else {
                    failures.push(ModelDiscoveryFailure::source("source_malformed", message));
                    complete = false;
                }
                continue;
            }
        };
        if !is_locally_served(&tag.name) {
            failures.push(ModelDiscoveryFailure::named(
                tag.name,
                "incomplete_facts",
                "the daemon relays this model upstream, so local unmetered facts do not apply",
            ));
            continue;
        }
        if embedding_only(&tag) {
            continue;
        }
        models.push(ModelDiscoveryEntry {
            slug: ModelSlug {
                provider: row.id().to_owned(),
                model: tag.name.clone(),
            },
            capabilities: capabilities_from(&tag, None),
            metering: MeteringKind::Unmetered,
            provenance: ModelFactProvenance::discovered(row.id().to_owned()),
        });
    }

    ModelDiscoveryReport::try_new(
        ModelDiscoverySource::Discovered {
            provider: row.id().to_owned(),
        },
        models,
        failures,
        complete,
    )
}

fn embedding_only(tag: &Tag) -> bool {
    tag.capabilities
        .iter()
        .any(|capability| capability == "embedding")
        && !tag
            .capabilities
            .iter()
            .any(|capability| capability == "completion")
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
        &row.http_profile,
        HttpRequest {
            url: url.clone(),
            method: Method::Get,
            headers: vec![public_header("accept", b"application/json".to_vec())],
            body: None,
        },
    )
    .await
    .map_err(|message| ResolveError::EndpointUnreachable {
        endpoint: url.to_string(),
        message,
    })?;

    let tags: Tags =
        serde_json::from_slice(&body).map_err(|err| ResolveError::EndpointUnreachable {
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
        &row.http_profile,
        HttpRequest {
            url: url.clone(),
            method: Method::Get,
            headers: vec![public_header("accept", b"application/json".to_vec())],
            body: None,
        },
    )
    .await
    .map_err(|message| ResolveError::EndpointUnreachable {
        endpoint: url.to_string(),
        message,
    })?;

    let ps: Ps =
        serde_json::from_slice(&body).map_err(|err| ResolveError::EndpointUnreachable {
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
        &row.http_profile,
        HttpRequest {
            url: url.clone(),
            method: Method::Post,
            headers: vec![public_header("content-type", b"application/json".to_vec())],
            body: Some(serde_json::to_vec(&body).expect("a two-key object of strings")),
        },
    )
    .await
    .map(|_| ())
    .map_err(|message| ResolveError::EndpointUnreachable {
        endpoint: url.to_string(),
        message,
    })
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
/// itself", and that was measured and found false: Ollama's
/// OpenAI-compatible endpoint honours `max_tokens` and ignores
/// `max_completion_tokens`, which is the field the family sends — correctly,
/// since `max_tokens` is deprecated at OpenAI and a shared wire format is not
/// where a provider's name belongs.
///
/// **It is true again, and by a different route.** The row names
/// [`wire::ollama`](crate::wire::ollama), a mapping of its own, which sends
/// the ceiling in the field this daemon reads
/// (`docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
/// §8). The request caps itself; `ollama_ceiling.rs`'s third recording is
/// `ego`'s own body against a live 0.32.14 daemon, stopping at the ceiling.
///
/// `max_output_tokens` stays `None` regardless, and the distinction is worth
/// keeping: this field is what the *daemon states*, and it states nothing. The
/// gate's `UnstatedCeiling { Output }` therefore still forces a request to
/// name one — which is now the thing that binds rather than the only thing
/// that happens.
fn capabilities_from(tag: &Tag, served_context: Option<u64>) -> ModelCapabilities {
    let thinks = tag.capabilities.iter().any(|cap| cap == "thinking");
    let sees_images = tag.capabilities.iter().any(|cap| cap == "vision");
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        max_context_tokens: served_context,
        max_output_tokens: None,
        max_tools: None,
        supports_parallel_tool_calls: false,
        image_input: if sees_images {
            ImageInputSupport::Supported(
                ImageLimits::new(
                    BTreeSet::from([
                        ImageMediaType::Jpeg,
                        ImageMediaType::Png,
                        ImageMediaType::Gif,
                        ImageMediaType::Webp,
                    ]),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .expect("Ollama vision media types are non-empty"),
            )
        } else {
            ImageInputSupport::Unsupported
        },
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
        // The daemon does not expose a normalized default for ego's actual
        // OpenAI-compatible mapping.
        default_reasoning_effort: None,
        // Dropping the connection stops the runner at a token boundary.
        cancel: CancelSemantics::Cooperative,
        // Ollama's prompt cache does not survive the model being unloaded,
        // which `keep_alive` does on a timer measured in minutes.
        cache: CacheRetention::Short,
        transport: Transport::Sse,
    }
}
