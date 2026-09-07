//! Authenticated current model authority for managed Kimi Code.

use std::collections::BTreeSet;
use std::sync::Arc;

use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ImageLimits, ModelCapabilities,
    ReasoningCarryover, ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::{ImageMediaType, ThinkingLevel};
use ego_core::public::MeteringKind;
use serde::Deserialize;

use crate::catalogue::{
    ModelDiscoveryEntry, ModelDiscoveryFailure, ModelDiscoveryReport, ModelDiscoverySource,
    ModelFactProvenance, ModelSlug, ResolveError, ResolvedModel, subscription,
};
use crate::http::{AdmittedHttpProfile, Http, HttpRequest, Method, exchange, public_header};
use crate::provider::{ProviderRow, ResolvedCredential};
use crate::wire::WireFormat;
use crate::wire::kimi_anthropic::DIALECT as KIMI_ANTHROPIC_DIALECT;
use crate::wire::kimi_anthropic::KimiAnthropic;
use crate::wire::kimi_openai::KimiOpenAi;

const MODELS_PATH: &str = "models";
const SUBSCRIPTION_REVISION: &str = "ego/builtin/kimi-coding-subscription/v1";

#[derive(Debug, Deserialize)]
struct ModelsEnvelope {
    data: Vec<Model>,
}

#[derive(Debug, Deserialize)]
struct Model {
    id: String,
    context_length: u64,
    supports_reasoning: bool,
    supports_image_in: bool,
    supports_video_in: bool,
    supports_tool_use: bool,
    #[serde(default)]
    supports_thinking_type: Option<String>,
    #[serde(default)]
    think_efforts: Option<ThinkEfforts>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    protocol: Protocol,
}

#[derive(Debug, Deserialize)]
struct ThinkEfforts {
    support: bool,
    #[serde(default)]
    valid_efforts: Vec<String>,
    #[serde(default)]
    default_effort: Option<String>,
}

#[derive(Debug, Default)]
enum Protocol {
    #[default]
    Missing,
    Value(serde_json::Value),
}

impl<'de> Deserialize<'de> for Protocol {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        serde_json::Value::deserialize(deserializer).map(Self::Value)
    }
}

struct CurrentModel {
    model: String,
    capabilities: ModelCapabilities,
    wire: &'static dyn WireFormat,
}

pub async fn resolve(
    row: &ProviderRow,
    requested: &str,
    http: &Arc<dyn Http>,
    credential: &ResolvedCredential,
) -> Result<ResolvedModel, ResolveError> {
    let models = current_models(row, http, credential).await?;
    let available = models
        .iter()
        .map(|model| model.model.clone())
        .collect::<Vec<_>>();
    let selected = models
        .into_iter()
        .find(|model| model.model == requested)
        .ok_or_else(|| ResolveError::NotInstalled {
            model: requested.to_string(),
            installed: available,
        })?;
    Ok(ResolvedModel {
        model: selected.model,
        row: row.clone(),
        wire: selected.wire,
        capabilities: selected.capabilities,
        metering: subscription(SUBSCRIPTION_REVISION),
        rates: None,
    })
}

pub async fn enumerate(
    row: &ProviderRow,
    http: &Arc<dyn Http>,
    credential: &ResolvedCredential,
) -> Result<ModelDiscoveryReport, String> {
    match current_models(row, http, credential).await {
        Ok(models) => ModelDiscoveryReport::try_new(
            ModelDiscoverySource::Discovered {
                provider: row.id().to_string(),
            },
            models
                .into_iter()
                .map(|model| ModelDiscoveryEntry {
                    slug: ModelSlug {
                        provider: row.id().to_string(),
                        model: model.model,
                    },
                    capabilities: model.capabilities,
                    metering: MeteringKind::Subscription,
                    provenance: ModelFactProvenance::discovered(row.id().to_string()),
                })
                .collect(),
            Vec::new(),
            true,
        ),
        Err(error) => ModelDiscoveryReport::try_new(
            ModelDiscoverySource::Discovered {
                provider: row.id().to_string(),
            },
            Vec::new(),
            vec![ModelDiscoveryFailure::source(
                discovery_cause(&error),
                error.to_string(),
            )],
            false,
        ),
    }
}

fn discovery_cause(error: &ResolveError) -> &'static str {
    match error {
        ResolveError::EndpointUnreachable { message, .. }
            if message.contains("parse") || message.contains("malformed") =>
        {
            "source_malformed"
        }
        ResolveError::Undiscoverable { .. } => "incomplete_facts",
        _ => "source_unreachable",
    }
}

async fn current_models(
    row: &ProviderRow,
    http: &Arc<dyn Http>,
    credential: &ResolvedCredential,
) -> Result<Vec<CurrentModel>, ResolveError> {
    let base = credential.base_override().unwrap_or(row.base());
    let url = base
        .join(MODELS_PATH)
        .map_err(|error| endpoint(row, base, format!("the effective base is invalid: {error}")))?;
    let profile = match credential.base_override() {
        Some(base) => AdmittedHttpProfile::from_urls(row.id(), std::slice::from_ref(base))
            .map_err(|error| endpoint(row, base, error.to_string()))?,
        None => row.http_profile.clone(),
    };
    let mut headers = credential.headers();
    for (name, value) in &row.extra_headers {
        headers.retain(|existing| !existing.name().eq_ignore_ascii_case(name));
        headers.push(public_header(name, value.as_bytes().to_vec()));
    }
    headers.push(public_header("accept", b"application/json".to_vec()));
    let answer = exchange(
        http,
        &profile,
        HttpRequest {
            url: url.clone(),
            method: Method::Get,
            headers,
            body: None,
        },
    )
    .await
    .map_err(|message| ResolveError::EndpointUnreachable {
        endpoint: format!("{} {}", row.id(), url),
        message,
    })?;
    if matches!(answer.status, 401..=403) {
        return Err(ResolveError::RejectedCredential {
            provider: row.id().to_string(),
            status: answer.status,
        });
    }
    if !(200..300).contains(&answer.status) {
        return Err(ResolveError::EndpointUnreachable {
            endpoint: format!("{} {}", row.id(), url),
            message: crate::http::refusal(&answer),
        });
    }
    let envelope: ModelsEnvelope = serde_json::from_slice(&answer.bytes).map_err(|error| {
        ResolveError::EndpointUnreachable {
            endpoint: format!("{} {}", row.id(), url),
            message: format!("its current models response did not parse: {error}"),
        }
    })?;
    envelope
        .data
        .into_iter()
        .map(|model| project(row, &url, model))
        .collect()
}

fn endpoint(row: &ProviderRow, base: &url::Url, message: String) -> ResolveError {
    ResolveError::EndpointUnreachable {
        endpoint: format!("{} {}models", row.id(), base),
        message,
    }
}

fn project(row: &ProviderRow, url: &url::Url, model: Model) -> Result<CurrentModel, ResolveError> {
    let malformed = |message: String| ResolveError::EndpointUnreachable {
        endpoint: format!("{} {url}", row.id()),
        message: format!("malformed current model {}: {message}", model.id),
    };
    if model.id.is_empty() {
        return Err(malformed("id is empty".to_string()));
    }
    if model.context_length == 0 {
        return Err(malformed("context_length must be positive".to_string()));
    }
    if model.supports_video_in && !model.supports_image_in {
        return Err(malformed(
            "video input is stated without image input".to_string(),
        ));
    }
    let (wire, reasoning_carryover, reasoning_state_dialect): (
        &'static dyn WireFormat,
        ReasoningCarryover,
        Option<String>,
    ) = match &model.protocol {
        Protocol::Missing => (&KimiOpenAi, ReasoningCarryover::Unsupported, None),
        Protocol::Value(serde_json::Value::String(protocol)) if protocol == "kimi" => {
            (&KimiOpenAi, ReasoningCarryover::Unsupported, None)
        }
        Protocol::Value(serde_json::Value::String(protocol)) if protocol == "anthropic" => (
            &KimiAnthropic,
            ReasoningCarryover::SameProviderOnly,
            Some(KIMI_ANTHROPIC_DIALECT.to_string()),
        ),
        Protocol::Value(serde_json::Value::String(protocol)) => {
            return Err(ResolveError::ProviderMappingUnavailable {
                provider: row.id().to_string(),
                mapping: protocol.clone(),
            });
        }
        Protocol::Value(_) => return Err(malformed("protocol must be a string".to_string())),
    };
    let efforts = reasoning_efforts(&model).map_err(malformed)?;
    let _display_name = model.display_name;
    Ok(CurrentModel {
        model: model.id,
        capabilities: ModelCapabilities {
            schema_version: ModelCapabilities::SCHEMA_VERSION,
            max_context_tokens: Some(model.context_length),
            max_output_tokens: None,
            max_tools: (!model.supports_tool_use).then_some(0),
            supports_parallel_tool_calls: false,
            image_input: if model.supports_image_in {
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
                    .expect("Kimi image media types are non-empty"),
                )
            } else {
                ImageInputSupport::Unsupported
            },
            reasoning_carryover,
            reasoning_efforts: efforts,
            reasoning_state_dialect,
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::None,
            transport: Transport::Sse,
        },
        wire,
    })
}

fn reasoning_efforts(model: &Model) -> Result<ReasoningEffortSupport, String> {
    match model.supports_thinking_type.as_deref() {
        Some("no") => return Ok(only([ThinkingLevel::Off])),
        Some("only" | "both") | None => {}
        Some(other) => return Err(format!("unknown supports_thinking_type {other}")),
    }
    if !model.supports_reasoning {
        return Ok(only([ThinkingLevel::Off]));
    }
    let Some(efforts) = &model.think_efforts else {
        return Ok(ReasoningEffortSupport::Any);
    };
    if !efforts.support {
        return Ok(only([ThinkingLevel::Off]));
    }
    let mut accepted = BTreeSet::new();
    if model.supports_thinking_type.as_deref() != Some("only") {
        accepted.insert(ThinkingLevel::Off);
    }
    for effort in &efforts.valid_efforts {
        accepted.insert(parse_effort(effort)?);
    }
    if accepted.is_empty() {
        return Err("think_efforts.support is true but valid_efforts is empty".to_string());
    }
    if let Some(default) = &efforts.default_effort {
        let default = parse_effort(default)?;
        if !accepted.contains(&default) {
            return Err("default_effort is absent from valid_efforts".to_string());
        }
    }
    Ok(ReasoningEffortSupport::Only(accepted))
}

fn only(levels: impl IntoIterator<Item = ThinkingLevel>) -> ReasoningEffortSupport {
    ReasoningEffortSupport::Only(levels.into_iter().collect())
}

fn parse_effort(value: &str) -> Result<ThinkingLevel, String> {
    match value {
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::XHigh),
        "max" => Ok(ThinkingLevel::Max),
        other => Err(format!("unknown thinking effort {other}")),
    }
}
