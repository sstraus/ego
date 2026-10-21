//! OpenRouter's current public model inventory, without imported prices.

use std::sync::Arc;

use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::ThinkingLevel;
use ego_core::public::MeteringKind;
use serde::Deserialize;

use crate::catalogue::{
    Catalogue, ModelDiscoveryEntry, ModelDiscoveryFailure, ModelDiscoveryReport,
    ModelDiscoverySource, ModelFactProvenance, ModelSlug, ResolveError, ResolvedModel,
    provider_reported,
};
use crate::http::{Http, HttpRequest, Method, exchange, public_header};
use crate::provider::{ProviderRow, ResolvedCredential};
use crate::wire::openai_completions::OpenRouterCompletions;

#[derive(Deserialize)]
struct Envelope {
    data: Vec<Model>,
}
#[derive(Deserialize)]
struct Model {
    id: String,
    context_length: Option<u64>,
    top_provider: Option<TopProvider>,
    #[serde(default)]
    supported_parameters: Vec<String>,
    reasoning: Option<Reasoning>,
}
#[derive(Deserialize)]
struct Reasoning {
    supported_efforts: Option<Vec<String>>,
    default_effort: Option<String>,
}

fn effort(value: &str) -> Option<ThinkingLevel> {
    match value {
        "none" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::XHigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

#[derive(Deserialize)]
struct TopProvider {
    max_completion_tokens: Option<u64>,
}

async fn inventory(
    row: &ProviderRow,
    http: &Arc<dyn Http>,
    credential: &ResolvedCredential,
) -> Result<Vec<ModelDiscoveryEntry>, String> {
    let url = row
        .base()
        .join("models")
        .map_err(|error| error.to_string())?;
    let mut headers = credential.headers();
    for (name, value) in &row.extra_headers {
        headers.retain(|header| !header.name().eq_ignore_ascii_case(name));
        headers.push(public_header(name, value.as_bytes().to_vec()));
    }
    headers.push(public_header("accept", b"application/json".to_vec()));
    let answer = exchange(
        http,
        &row.http_profile,
        HttpRequest {
            url,
            method: Method::Get,
            headers,
            body: None,
        },
    )
    .await?;
    if !(200..300).contains(&answer.status) {
        return Err(crate::http::refusal(&answer));
    }
    let envelope: Envelope = serde_json::from_slice(&answer.bytes)
        .map_err(|error| format!("OpenRouter models response did not parse: {error}"))?;
    envelope
        .data
        .into_iter()
        .map(|model| {
            if model.id.trim().is_empty() {
                return Err("OpenRouter models response contains an empty id".to_owned());
            }
            let parameters = model.supported_parameters;
            let has_reasoning = parameters.iter().any(|parameter| parameter == "reasoning");
            let reasoning_efforts = if has_reasoning {
                model
                    .reasoning
                    .as_ref()
                    .and_then(|reasoning| reasoning.supported_efforts.as_ref())
                    .map(|levels| {
                        ReasoningEffortSupport::Only(
                            levels.iter().filter_map(|level| effort(level)).collect(),
                        )
                    })
                    .unwrap_or(ReasoningEffortSupport::Unknown)
            } else {
                ReasoningEffortSupport::Only([ThinkingLevel::Off].into())
            };
            let default_reasoning_effort = if has_reasoning {
                model
                    .reasoning
                    .as_ref()
                    .and_then(|reasoning| reasoning.default_effort.as_deref())
                    .and_then(effort)
            } else {
                Some(ThinkingLevel::Off)
            };
            Ok(ModelDiscoveryEntry {
                slug: ModelSlug {
                    provider: row.id().to_owned(),
                    model: model.id,
                },
                capabilities: ModelCapabilities {
                    schema_version: ModelCapabilities::SCHEMA_VERSION,
                    audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
                    max_context_tokens: model.context_length,
                    max_output_tokens: model
                        .top_provider
                        .and_then(|provider| provider.max_completion_tokens),
                    max_tools: None,
                    supports_parallel_tool_calls: parameters
                        .iter()
                        .any(|parameter| parameter == "parallel_tool_calls"),
                    reasoning_carryover: ReasoningCarryover::Unsupported,
                    reasoning_efforts,
                    default_reasoning_effort,
                    reasoning_state_dialect: None,
                    cancel: CancelSemantics::Unsupported,
                    cache: CacheRetention::None,
                    transport: Transport::Sse,
                    image_input: ImageInputSupport::Unknown,
                },
                metering: MeteringKind::Currency,
                provenance: ModelFactProvenance::discovered(row.id().to_owned()),
            })
        })
        .collect()
}

pub async fn enumerate(
    row: &ProviderRow,
    http: &Arc<dyn Http>,
    credential: &ResolvedCredential,
) -> Result<ModelDiscoveryReport, String> {
    match inventory(row, http, credential).await {
        Ok(models) => ModelDiscoveryReport::try_new(
            ModelDiscoverySource::Discovered {
                provider: row.id().to_owned(),
            },
            models,
            Vec::new(),
            true,
        ),
        Err(message) => ModelDiscoveryReport::try_new(
            ModelDiscoverySource::Discovered {
                provider: row.id().to_owned(),
            },
            Vec::new(),
            vec![ModelDiscoveryFailure::source(
                if message.contains("parse") || message.contains("empty id") {
                    "source_malformed"
                } else {
                    "source_unreachable"
                },
                message,
            )],
            false,
        ),
    }
}

pub async fn resolve(
    catalogue: &Catalogue,
    row: &ProviderRow,
    model: &str,
    http: &Arc<dyn Http>,
    credential: &ResolvedCredential,
) -> Result<ResolvedModel, ResolveError> {
    let models = inventory(row, http, credential).await.map_err(|message| {
        ResolveError::EndpointUnreachable {
            endpoint: format!("{} models", row.id()),
            message,
        }
    })?;
    let installed = models
        .iter()
        .map(|entry| entry.slug.model.clone())
        .collect();
    // An operator row in `models.toml` overrides discovery, including reasoning
    // facts, and can also describe an unlisted model.
    let mut capabilities = match catalogue.supplied_capabilities(&format!("{}/{model}", row.id())) {
        Some(supplied) => supplied.clone(),
        None => {
            models
                .into_iter()
                .find(|entry| entry.slug.model == model)
                .ok_or_else(|| ResolveError::NotInstalled {
                    model: model.to_owned(),
                    installed,
                })?
                .capabilities
        }
    };
    capabilities.audio_input = ego_core::generator::capabilities::AudioInputSupport::Unsupported;
    Ok(ResolvedModel {
        model: model.to_owned(),
        row: row.clone(),
        wire: &OpenRouterCompletions,
        capabilities,
        metering: provider_reported(
            row.cost
                .as_ref()
                .expect("OpenRouter owns reported cost")
                .as_ref(),
        ),
        rates: catalogue
            .supplied_rates(&format!("{}/{model}", row.id()))
            .cloned(),
    })
}
