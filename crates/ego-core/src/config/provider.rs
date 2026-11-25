//! Trusted deployment identity and complete, non-secret model bindings.
use std::collections::BTreeMap;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::generator::capabilities::ModelCapabilities;

/// A stable selection/reference key, never an endpoint or a model route.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProviderInstanceId(String);

impl ProviderInstanceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ProviderInstanceId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !(1..=64).contains(&value.len())
            || !value
                .bytes()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            || !value.bytes().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_' | b'.')
            })
        {
            return Err("provider instance keys require [a-z0-9][a-z0-9._-]{0,63}".into());
        }
        Ok(Self(value.to_owned()))
    }
}

impl<'de> Deserialize<'de> for ProviderInstanceId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Vendor and configured profile for Azure's public-cloud OpenAI v1 routes.
pub const AZURE_OPENAI: &str = "azure-openai";

/// A credential-free deployment base. Plain HTTP is restricted to exact loopback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEndpoint(Url);

impl ProviderEndpoint {
    pub fn as_url(&self) -> &Url {
        &self.0
    }

    /// Admit only the documented public-cloud resource host and v1 base.
    pub fn is_azure_openai_v1(&self) -> bool {
        let url = &self.0;
        url.scheme() == "https"
            && url.port().is_none()
            && url.path() == "/openai/v1/"
            && url.domain().is_some_and(|host| {
                [".openai.azure.com", ".services.ai.azure.com"]
                    .iter()
                    .filter_map(|suffix| host.strip_suffix(suffix))
                    .any(|resource| {
                        !resource.is_empty()
                            && !resource.starts_with('-')
                            && !resource.ends_with('-')
                            && resource
                                .bytes()
                                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
                    })
            })
    }

    pub fn is_exact_loopback(&self) -> bool {
        match self.0.host() {
            Some(Host::Domain(host)) => host == "localhost",
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        }
    }
}

impl FromStr for ProviderEndpoint {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut url = Url::parse(value).map_err(|_| "invalid provider base URL".to_owned())?;
        let loopback = match url.host() {
            Some(Host::Domain(host)) => host == "localhost",
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        };
        let authority = value
            .split_once("://")
            .map(|(_, tail)| tail.split(['/', '?', '#']).next().unwrap_or_default());
        if url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || authority.is_some_and(|authority| authority.contains('@'))
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        {
            // Do not echo a rejected URL: it may contain a credential.
            return Err("provider base requires HTTPS (or exact loopback HTTP), without userinfo, query or fragment".into());
        }
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        Ok(Self(url))
    }
}

impl Serialize for ProviderEndpoint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for ProviderEndpoint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Explicit mappings compiled into this build, never inferred from an alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderModelContract {
    ChatStandard,
    /// Text/client-tool Chat with `max_tokens`, without reasoning or parallel controls.
    ChatMaxTokens,
    Responses,
    Messages,
    GeminiInteractions,
    Deepseek,
    /// Native text/client-tool Converse, with binary tool-free streaming.
    BedrockConverse,
    /// Bedrock Runtime's Chat Completions route for a reviewed model binding.
    BedrockChat,
    /// Exact GLM-5.3 reasoning Chat contract.
    Zai,
    /// Exact Kimi K3.1 API-key reasoning Chat contract.
    KimiApi,
    /// Moonshot Responses with stateless replay and provider-hosted search.
    KimiResponses,
    /// Mistral's stateless Conversations API, not Chat Completions.
    MistralConversations,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInstanceModel {
    pub upstream_id: String,
    pub contract: ProviderModelContract,
    pub facts: ModelCapabilities,
    pub metering: crate::budget::MeteringIdentity,
}

impl ProviderInstanceModel {
    pub fn validate(&self) -> Result<(), String> {
        if matches!(
            self.contract,
            ProviderModelContract::ChatMaxTokens | ProviderModelContract::BedrockChat
        ) {
            use crate::generator::capabilities::{
                AudioInputSupport, CacheRetention, ImageInputSupport, ReasoningCarryover,
                ReasoningEffortSupport, Transport,
            };
            use crate::generator::types::ThinkingLevel;
            if self.facts.reasoning_efforts
                != ReasoningEffortSupport::Only([ThinkingLevel::Off].into())
                || self.facts.default_reasoning_effort != Some(ThinkingLevel::Off)
                || self.facts.reasoning_carryover != ReasoningCarryover::Unsupported
                || self.facts.reasoning_state_dialect.is_some()
                || self.facts.supports_parallel_tool_calls
                || self.facts.max_tools.is_none()
                || self.facts.cache != CacheRetention::None
                || self.facts.transport != Transport::Sse
                || matches!(self.facts.image_input, ImageInputSupport::Supported(_))
                || self.facts.audio_input == AudioInputSupport::SupportedWav
            {
                return Err("chat-max-tokens requires explicit off-only reasoning and tool limits, no replay/parallel/cache/modalities, and SSE facts".into());
            }
        }
        if self.contract == ProviderModelContract::BedrockConverse {
            use crate::generator::capabilities::{
                AudioInputSupport, CacheRetention, ImageInputSupport, ReasoningCarryover,
                ReasoningEffortSupport, Transport,
            };
            use crate::generator::types::ThinkingLevel;
            if self.facts.reasoning_efforts
                != ReasoningEffortSupport::Only([ThinkingLevel::Off].into())
                || self.facts.default_reasoning_effort != Some(ThinkingLevel::Off)
                || self.facts.reasoning_carryover != ReasoningCarryover::Unsupported
                || self.facts.reasoning_state_dialect.is_some()
                || self.facts.cache != CacheRetention::None
                || self.facts.transport != Transport::AmazonEventStream
                || self.facts.image_input != ImageInputSupport::Unsupported
                || self.facts.audio_input != AudioInputSupport::Unsupported
                || self.facts.max_tools.is_none()
            {
                return Err("bedrock-converse requires explicit off-only reasoning, no replay/cache/modalities, native eventstream and explicit tool facts".into());
            }
        }
        if matches!(
            self.contract,
            ProviderModelContract::Zai
                | ProviderModelContract::KimiApi
                | ProviderModelContract::KimiResponses
        ) {
            use crate::generator::capabilities::{
                AudioInputSupport, CacheRetention, ImageInputSupport, ReasoningCarryover,
                ReasoningEffortSupport, Transport,
            };
            use crate::generator::types::ThinkingLevel;
            let (dialect, output_limit) = match self.contract {
                ProviderModelContract::Zai => ("zai-glm53-chat", 131_072),
                ProviderModelContract::KimiApi => ("kimi-api-k3-chat", 1_048_576),
                ProviderModelContract::KimiResponses => ("kimi-responses", 1_048_576),
                _ => unreachable!("the exact reasoning contracts are selected above"),
            };
            if self.facts.reasoning_efforts
                != ReasoningEffortSupport::Only(
                    [ThinkingLevel::Low, ThinkingLevel::High, ThinkingLevel::Max].into(),
                )
                || self.facts.default_reasoning_effort != Some(ThinkingLevel::Max)
                || self.facts.reasoning_carryover != ReasoningCarryover::SameProviderOnly
                || self.facts.reasoning_state_dialect.as_deref() != Some(dialect)
                || self.facts.supports_parallel_tool_calls
                || self.facts.max_tools.is_none()
                || (self.contract == ProviderModelContract::Zai
                    && self.facts.max_tools.is_some_and(|limit| limit > 128))
                || self
                    .facts
                    .max_output_tokens
                    .is_some_and(|limit| limit > output_limit)
                || (self.contract != ProviderModelContract::KimiResponses
                    && self.facts.cache != CacheRetention::None)
                || self.facts.transport != Transport::Sse
                || self.facts.image_input != ImageInputSupport::Unsupported
                || self.facts.audio_input != AudioInputSupport::Unsupported
            {
                return Err("exact reasoning contracts require low/high/max with default max, matching provider-only replay, explicit bounded output/tool facts, supported cache settings, no parallel calls or modalities, and SSE".into());
            }
        }
        if self.contract == ProviderModelContract::MistralConversations {
            use crate::generator::capabilities::{
                AudioInputSupport, CacheRetention, ImageInputSupport, ReasoningCarryover, Transport,
            };
            if self.facts.reasoning_carryover != ReasoningCarryover::SameProviderOnly
                || self.facts.reasoning_state_dialect.as_deref() != Some("mistral-conversations")
                || self.facts.cache != CacheRetention::None
                || self.facts.transport != Transport::Sse
                || self.facts.image_input != ImageInputSupport::Unsupported
                || self.facts.audio_input != AudioInputSupport::Unsupported
                || self.facts.supports_parallel_tool_calls
            {
                return Err("mistral-conversations requires same-provider replay, SSE, and no cache controls, modalities or parallel-call control".into());
            }
        }
        if self.facts.schema_version != ModelCapabilities::SCHEMA_VERSION {
            return Err("instance model facts require the current capability schema".into());
        }
        if matches!(
            self.metering,
            crate::budget::MeteringIdentity::Currency {
                settlement: crate::budget::SettlementSource::ProviderReported { .. }
            }
        ) {
            return Err(
                "instance models require an explicit quote or non-currency metering policy".into(),
            );
        }
        if self.upstream_id.trim().is_empty()
            || self.upstream_id.len() > 512
            || self.upstream_id.chars().any(char::is_control)
        {
            return Err("instance upstream model requires 1..512 non-control bytes".into());
        }
        let (Some(context), Some(output)) =
            (self.facts.max_context_tokens, self.facts.max_output_tokens)
        else {
            return Err("instance model requires explicit context and output token facts".into());
        };
        if context == 0 || output == 0 || output > context {
            return Err(
                "instance model token facts must be positive and output must fit context".into(),
            );
        }
        Ok(())
    }
}

/// Compiled deployment profiles whose endpoint/auth presentation is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderProfile {
    OpenaiCompatible,
    Openai,
    AzureOpenai,
    Anthropic,
    Gemini,
    /// Explicit project/location-bound Gemini Interactions on Google Cloud.
    VertexGemini,
    Deepseek,
    BedrockRuntime,
    BedrockRuntimeSigv4,
    Zai,
    KimiApi,
    Mistral,
}

impl ProviderProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiCompatible => "openai-compatible",
            Self::Openai => "openai",
            Self::AzureOpenai => AZURE_OPENAI,
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::VertexGemini => "vertex-gemini",
            Self::Deepseek => "deepseek",
            Self::BedrockRuntime => "bedrock-runtime",
            Self::BedrockRuntimeSigv4 => "bedrock-runtime-sigv4",
            Self::Zai => "zai",
            Self::KimiApi => "kimi-api",
            Self::Mistral => "mistral",
        }
    }

    pub const fn allows(self, contract: ProviderModelContract) -> bool {
        use ProviderModelContract as Contract;
        matches!(
            (self, contract),
            (
                Self::Openai | Self::OpenaiCompatible | Self::AzureOpenai,
                Contract::Responses | Contract::ChatStandard
            ) | (Self::OpenaiCompatible, Contract::ChatMaxTokens)
                | (Self::Anthropic, Contract::Messages)
                | (
                    Self::Gemini | Self::VertexGemini,
                    Contract::GeminiInteractions
                )
                | (Self::Deepseek, Contract::Deepseek)
                | (
                    Self::BedrockRuntime | Self::BedrockRuntimeSigv4,
                    Contract::BedrockConverse | Contract::BedrockChat
                )
                | (Self::Zai, Contract::Zai)
                | (Self::KimiApi, Contract::KimiApi | Contract::KimiResponses)
                | (Self::Mistral, Contract::MistralConversations)
        )
    }
}

impl FromStr for ProviderProfile {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai-compatible" => Ok(Self::OpenaiCompatible),
            "openai" => Ok(Self::Openai),
            AZURE_OPENAI => Ok(Self::AzureOpenai),
            "anthropic" => Ok(Self::Anthropic),
            "gemini" => Ok(Self::Gemini),
            "vertex-gemini" => Ok(Self::VertexGemini),
            "deepseek" => Ok(Self::Deepseek),
            "bedrock-runtime" => Ok(Self::BedrockRuntime),
            "bedrock-runtime-sigv4" => Ok(Self::BedrockRuntimeSigv4),
            "zai" => Ok(Self::Zai),
            "kimi-api" => Ok(Self::KimiApi),
            "mistral" => Ok(Self::Mistral),
            _ => Err("unsupported provider instance profile".into()),
        }
    }
}

/// One indivisible trusted endpoint/auth/model authority. Secrets are elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInstance {
    pub profile: ProviderProfile,
    pub base_url: ProviderEndpoint,
    /// Stable user-owned connection ID; absent only for unauthenticated profiles.
    pub credential_ref: Option<ProviderInstanceId>,
    pub models: BTreeMap<ProviderInstanceId, ProviderInstanceModel>,
}

impl ProviderInstance {
    pub fn validate(&self) -> Result<(), String> {
        if self.profile == ProviderProfile::VertexGemini {
            self.validate_vertex_gemini()?;
        }
        if self.credential_ref.is_none()
            && !(self.profile == ProviderProfile::OpenaiCompatible
                && self.base_url.is_exact_loopback())
        {
            return Err(
                "authenticated provider instance requires an explicit credential reference".into(),
            );
        }
        if self.profile == ProviderProfile::AzureOpenai && !self.base_url.is_azure_openai_v1() {
            return Err(
                "azure-openai requires a public Azure resource HTTPS base ending in /openai/v1/"
                    .into(),
            );
        }
        if self.models.is_empty() {
            return Err("provider instance requires at least one explicit model binding".into());
        }
        for model in self.models.values() {
            if !self.profile.allows(model.contract) {
                return Err("model contract is not admitted by the instance profile".into());
            }
            if model.contract == ProviderModelContract::BedrockChat
                && (model.upstream_id != "mistral.mistral-large-3-675b-instruct"
                    || self.base_url.as_url().as_str()
                        != "https://bedrock-runtime.us-east-1.amazonaws.com/")
            {
                return Err(
                    "bedrock-chat requires Mistral Large 3 on the AWS us-east-1 runtime root"
                        .into(),
                );
            }
            model.validate()?;
        }
        Ok(())
    }

    /// Only the published foreground Interactions subset is admitted. The URL
    /// itself is the explicit project/location authority, never ambient context.
    fn validate_vertex_gemini(&self) -> Result<(), String> {
        let url = self.base_url.as_url();
        let valid_route = url
            .path()
            .strip_prefix("/v1/projects/")
            .is_some_and(|route| {
                let parts: Vec<_> = route.split('/').collect();
                let [project, "locations", location, ""] = parts.as_slice() else {
                    return false;
                };
                let project_valid = !project.is_empty()
                    && !project.starts_with(['-', '.'])
                    && project.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'-' | b'.' | b':')
                    });
                let host = match *location {
                    "global" => Some("aiplatform.googleapis.com"),
                    "us" => Some("aiplatform.us.rep.googleapis.com"),
                    "eu" => Some("aiplatform.eu.rep.googleapis.com"),
                    _ => None,
                };
                project_valid && host.is_some() && url.host_str() == host
            });
        if !valid_route || url.scheme() != "https" || url.port().is_some() {
            return Err("vertex-gemini requires an explicit project and global/us/eu location at its matching published v1 Interactions root".into());
        }
        use crate::generator::capabilities::{
            AudioInputSupport, CacheRetention, ReasoningCarryover, ReasoningEffortSupport,
            Transport,
        };
        use crate::generator::types::ThinkingLevel;
        for model in self.models.values() {
            // This is an allowlisted model contract, not a default model or a
            // discovery claim. Larger output requires unsupported continuation.
            if model.upstream_id != "gemini-4-argon"
                || model
                    .facts
                    .max_context_tokens
                    .is_none_or(|limit| limit > 1_048_576)
                || model
                    .facts
                    .max_output_tokens
                    .is_none_or(|limit| limit > 65_536)
                || model.facts.reasoning_efforts
                    != ReasoningEffortSupport::Only(
                        [
                            ThinkingLevel::Low,
                            ThinkingLevel::Medium,
                            ThinkingLevel::High,
                        ]
                        .into(),
                    )
                || model.facts.default_reasoning_effort != Some(ThinkingLevel::Medium)
                || model.facts.reasoning_carryover != ReasoningCarryover::SameProviderOnly
                || model.facts.reasoning_state_dialect.as_deref() != Some("gemini-interactions")
                || model.facts.transport != Transport::Sse
                || model.facts.cache != CacheRetention::None
                || model.facts.audio_input != AudioInputSupport::Unsupported
                || model.facts.max_tools.is_none()
            {
                return Err("vertex-gemini requires explicit gemini-4-argon foreground facts: bounded context/output, low/medium/high with medium default, provider-only Interactions replay, SSE, no cache/audio and explicit tool limits".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersLayer {
    #[serde(default)]
    pub instances: BTreeMap<ProviderInstanceId, ProviderInstance>,
}

/// Trusted effective instances retain their declaring authority.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedProviders {
    pub instances: BTreeMap<ProviderInstanceId, super::Sourced<ProviderInstance>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigLayer, Source, Sourced, Trust, resolve};
    use crate::generator::capabilities::*;

    fn instance(upstream: &str) -> ProviderInstance {
        ProviderInstance {
            profile: "openai-compatible".parse().unwrap(),
            base_url: "https://inference.example/prefix/v1".parse().unwrap(),
            credential_ref: Some("office".parse().unwrap()),
            models: BTreeMap::from([(
                "assistant".parse().unwrap(),
                ProviderInstanceModel {
                    upstream_id: upstream.into(),
                    contract: ProviderModelContract::ChatStandard,
                    metering: crate::budget::MeteringIdentity::Currency {
                        settlement: crate::budget::SettlementSource::Quote,
                    },
                    facts: ModelCapabilities {
                        schema_version: ModelCapabilities::SCHEMA_VERSION,
                        max_context_tokens: Some(32000),
                        max_output_tokens: Some(2048),
                        max_tools: Some(8),
                        supports_parallel_tool_calls: false,
                        reasoning_carryover: ReasoningCarryover::Unsupported,
                        reasoning_efforts: ReasoningEffortSupport::Unknown,
                        default_reasoning_effort: None,
                        reasoning_state_dialect: None,
                        cancel: CancelSemantics::Cooperative,
                        cache: CacheRetention::None,
                        transport: Transport::Sse,
                        image_input: ImageInputSupport::Unknown,
                        audio_input: AudioInputSupport::Unknown,
                    },
                },
            )]),
        }
    }

    // Catches exact profile bindings advertising reasoning/cache/transport facts
    // that their native request and replay adapters cannot serve.
    #[test]
    fn exact_reasoning_contracts_reject_inexpressible_instance_facts() {
        use crate::generator::types::ThinkingLevel;
        for (profile, contract, dialect, output_limit) in [
            (
                ProviderProfile::Zai,
                ProviderModelContract::Zai,
                "zai-glm53-chat",
                131_072,
            ),
            (
                ProviderProfile::KimiApi,
                ProviderModelContract::KimiApi,
                "kimi-api-k3-chat",
                1_048_576,
            ),
            (
                ProviderProfile::KimiApi,
                ProviderModelContract::KimiResponses,
                "kimi-responses",
                1_048_576,
            ),
        ] {
            let mut deployment = instance("operator-declared-model");
            deployment.profile = profile;
            let model = deployment.models.values_mut().next().unwrap();
            model.contract = contract;
            model.facts.max_context_tokens = Some(output_limit * 2);
            model.facts.max_output_tokens = Some(output_limit);
            model.facts.reasoning_efforts = ReasoningEffortSupport::Only(
                [ThinkingLevel::Low, ThinkingLevel::High, ThinkingLevel::Max].into(),
            );
            model.facts.default_reasoning_effort = Some(ThinkingLevel::Max);
            model.facts.reasoning_carryover = ReasoningCarryover::SameProviderOnly;
            model.facts.reasoning_state_dialect = Some(dialect.into());
            model.facts.image_input = ImageInputSupport::Unsupported;
            model.facts.audio_input = AudioInputSupport::Unsupported;
            assert!(deployment.validate().is_ok());
            let encoded = serde_json::to_value(&deployment).unwrap();
            let decoded: ProviderInstance = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded, deployment);
            for change in 0..9 {
                let mut invalid = deployment.clone();
                let facts = &mut invalid.models.values_mut().next().unwrap().facts;
                match change {
                    0 => facts.default_reasoning_effort = Some(ThinkingLevel::Off),
                    1 => facts.reasoning_efforts = ReasoningEffortSupport::Any,
                    2 => facts.reasoning_state_dialect = Some("foreign-chat".into()),
                    3 => facts.reasoning_carryover = ReasoningCarryover::Portable,
                    4 => facts.supports_parallel_tool_calls = true,
                    5 => facts.max_tools = None,
                    6 => facts.cache = CacheRetention::Long,
                    7 => facts.transport = Transport::WebSocket,
                    _ => facts.max_output_tokens = Some(output_limit + 1),
                }
                if change == 6 && contract == ProviderModelContract::KimiResponses {
                    assert!(invalid.validate().is_ok());
                } else {
                    assert!(invalid.validate().is_err(), "{contract:?}: change {change}");
                }
            }
            let mut foreign = deployment.clone();
            foreign.profile = if profile == ProviderProfile::Zai {
                ProviderProfile::KimiApi
            } else {
                ProviderProfile::Zai
            };
            assert!(foreign.validate().is_err());
            if profile == ProviderProfile::Zai {
                deployment
                    .models
                    .values_mut()
                    .next()
                    .unwrap()
                    .facts
                    .max_tools = Some(129);
                assert!(deployment.validate().is_err());
            }
        }
    }

    // Catches credential-bearing URLs reaching parsing diagnostics or admission.
    #[test]
    fn provider_instance_routes_reject_secret_authority_and_plaintext_nonloopback() {
        for route in [
            "https://user:private-token@example.com/v1",
            "https://@example.com/v1",
            "https://example.com/?key=private-token",
            "https://example.com/#private-token",
            "http://localhost.example/v1",
            "file:///private-token",
        ] {
            let error = route.parse::<ProviderEndpoint>().unwrap_err();
            assert!(!error.contains("private-token"));
        }
        for route in [
            "http://127.0.0.1:1234/v1",
            "http://[::1]:1234/v1",
            "https://example.com/prefix/v1",
        ] {
            assert!(
                route
                    .parse::<ProviderEndpoint>()
                    .unwrap()
                    .as_url()
                    .path()
                    .ends_with('/')
            );
        }
    }

    // Catches trusted-workspace or direct-layer injection laundering endpoint/auth authority.
    #[test]
    fn provider_instance_rejects_untrusted_source_even_when_workspace_is_trusted() {
        let providers = ProvidersLayer {
            instances: BTreeMap::from([("office".parse().unwrap(), instance("deployment-a"))]),
        };
        #[derive(Serialize)]
        struct Document<'a> {
            providers: &'a ProvidersLayer,
        }
        let text = toml::to_string(&Document {
            providers: &providers,
        })
        .unwrap();
        for source in [Source::Project, Source::Local, Source::Env, Source::Flags] {
            assert!(ConfigLayer::parse(source, &text).is_err());
            for trust in [Trust::Trusted, Trust::Untrusted] {
                assert!(
                    resolve(
                        &[Sourced {
                            source,
                            value: ConfigLayer {
                                providers: providers.clone(),
                                ..Default::default()
                            }
                        }],
                        trust,
                        None
                    )
                    .is_err()
                );
            }
        }
    }

    // Catches same-profile IDs collapsing, fieldwise route/auth merging, and unknown contract acceptance.
    #[test]
    fn provider_instance_roundtrip_is_independent_and_duplicate_authority_fails() {
        let providers = ProvidersLayer {
            instances: BTreeMap::from([
                ("office".parse().unwrap(), instance("deployment-a")),
                ("home".parse().unwrap(), instance("deployment-b")),
            ]),
        };
        #[derive(Serialize)]
        struct Document<'a> {
            providers: &'a ProvidersLayer,
        }
        let text = toml::to_string(&Document {
            providers: &providers,
        })
        .unwrap();
        let secret_route = text.replace(
            "https://inference.example/prefix/v1/",
            "https://user:private-token@example.com/v1",
        );
        assert!(
            !ConfigLayer::parse(Source::User, &secret_route)
                .unwrap_err()
                .to_string()
                .contains("private-token")
        );
        let user = ConfigLayer::parse(Source::User, &text).unwrap();
        let effective = resolve(std::slice::from_ref(&user), Trust::Trusted, None).unwrap();
        assert_eq!(effective.providers.instances.len(), 2);
        assert_eq!(
            effective.providers.instances[&"office".parse().unwrap()]
                .value
                .models[&"assistant".parse().unwrap()]
                .upstream_id,
            "deployment-a"
        );
        assert_eq!(
            effective.providers.instances[&"home".parse().unwrap()]
                .value
                .models[&"assistant".parse().unwrap()]
                .upstream_id,
            "deployment-b"
        );
        let system = ConfigLayer::parse(Source::System, &text).unwrap();
        assert!(resolve(&[system, user], Trust::Trusted, None).is_err());
        assert!(
            ConfigLayer::parse(
                Source::User,
                &text.replace("chat-standard", "arbitrary-transform")
            )
            .is_err()
        );
        assert!(
            ConfigLayer::parse(
                Source::User,
                &text.replace("upstream_id =", "arbitrary_json =")
            )
            .is_err()
        );
    }
    // Catches silently ignored facts typos without breaking older persisted optional-field defaults.
    #[test]
    fn provider_instance_facts_are_strict_but_old_native_records_keep_unknown_defaults() {
        let model = instance("deployment-a")
            .models
            .remove(&"assistant".parse().unwrap())
            .unwrap();
        let mut value = serde_json::to_value(&model.facts).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unreviewed_capability".into(), serde_json::json!(true));
        assert!(serde_json::from_value::<ModelCapabilities>(value.clone()).is_err());
        let object = value.as_object_mut().unwrap();
        object.remove("unreviewed_capability");
        for field in [
            "reasoning_efforts",
            "default_reasoning_effort",
            "reasoning_state_dialect",
            "image_input",
            "audio_input",
        ] {
            object.remove(field);
        }
        object.insert("schema_version".into(), serde_json::json!(1));
        let old: ModelCapabilities = serde_json::from_value(value).unwrap();
        assert_eq!(old.schema_version, 1);
        assert_eq!(old.reasoning_efforts, ReasoningEffortSupport::Unknown);
        assert_eq!(old.reasoning_state_dialect, None);
        assert_eq!(old.default_reasoning_effort, None);
        assert_eq!(old.image_input, ImageInputSupport::Unknown);
        assert_eq!(old.audio_input, AudioInputSupport::Unknown);
        assert_eq!(old.max_context_tokens, Some(32000));
    }
}
