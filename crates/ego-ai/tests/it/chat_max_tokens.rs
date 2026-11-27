//! Request expectations are derived from reviewed public schemas, not captures.

use std::collections::BTreeMap;

use base64::Engine as _;

use ego_ai::catalogue::{Catalogue, ModelSlug};
use ego_ai::provider::ProviderRow;
use ego_ai::wire::chat_max_tokens::ChatMaxTokens;
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::config::{
    ProviderInstance, ProviderInstanceModel, ProviderModelContract, ProviderProfile,
    ResolvedProviders, Source, Sourced,
};
use ego_core::generator::capabilities::*;
use ego_core::generator::types::{
    ContentBlock, Conversation, GenRequest, HostedSearch, Message, PromptCache, ReasoningState,
    Role, ThinkingLevel, ToolDescriptor,
};
use serde_json::json;
use url::Url;

fn request() -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: "deployed-nonreasoning-model".into(),
        messages: Conversation::new(vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".into(),
            }],
        }])
        .unwrap(),
        tools: vec![],
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(19),
        parallel_tool_calls: false,
    }
}

fn body(req: &GenRequest) -> serde_json::Value {
    let base = Url::parse("http://127.0.0.1:1234/prefix/v1/").unwrap();
    let wire = ChatMaxTokens
        .build_request(&RequestTarget::new(&base, &[]), req)
        .unwrap();
    assert_eq!(
        wire.url.as_str(),
        "http://127.0.0.1:1234/prefix/v1/chat/completions"
    );
    serde_json::from_slice(wire.body.as_deref().unwrap()).unwrap()
}

// Catches inheriting the OpenAI ceiling or undocumented reasoning/usage controls.
#[test]
fn exact_ceiling_request_has_no_unproven_controls() {
    let mut req = request();
    assert_eq!(
        body(&req),
        json!({
            "model": "deployed-nonreasoning-model",
            "messages": [{"role":"user", "content":"Hello"}],
            "stream": true,
            "max_tokens": 19
        })
    );
    req.max_output_tokens = None;
    assert_eq!(
        body(&req),
        json!({
            "model": "deployed-nonreasoning-model",
            "messages": [{"role":"user", "content":"Hello"}],
            "stream": true
        })
    );
}

// Catches dropping caller controls or leaking foreign opaque state to a compatible shape.
#[test]
fn unsupported_controls_and_state_refuse_before_dispatch() {
    let base = Url::parse("https://example.com/v1/").unwrap();
    let target = RequestTarget::new(&base, &[]);
    let mut cases = Vec::new();
    for level in [
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::XHigh,
        ThinkingLevel::Max,
    ] {
        let mut req = request();
        req.thinking = level;
        cases.push((format!("effort {level:?}"), req));
    }
    let mut parallel = request();
    parallel.parallel_tool_calls = true;
    parallel.tools = vec![ToolDescriptor {
        name: "lookup".into(),
        description: "Look up a value".into(),
        parameters: json!({"type":"object"}),
    }];
    cases.push(("parallel control".into(), parallel));
    let mut search = request();
    search.web_search = Some(HostedSearch {
        max_requests: std::num::NonZeroU32::new(1).unwrap(),
        allow_live_access: false,
    });
    cases.push(("hosted search".into(), search));
    let mut cache = request();
    cache.cache = Some(PromptCache {
        verifier: None,
        session_id: ego_core::event::id::SessionId::new(),
        lineage_root: ego_core::event::id::SessionId::new(),
        retention: CacheRetention::Short,
        stable_messages: 1,
    });
    cases.push(("explicit short cache".into(), cache.clone()));
    cache.cache.as_mut().unwrap().retention = CacheRetention::Long;
    cases.push(("explicit long cache".into(), cache));
    for dialect in ["deepseek-chat", "openrouter-chat", "chat-max-tokens"] {
        let mut req = request();
        req.messages = Conversation::new(vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Reasoning {
                state: ReasoningState {
                    connection: None,
                    blob: "opaque".into(),
                    dialect: dialect.into(),
                },
            }],
        }])
        .unwrap();
        cases.push((format!("state {dialect}"), req));
    }
    let mut provider_content = request();
    provider_content.messages = Conversation::new(vec![Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ProviderContent {
            state: ReasoningState {
                connection: None,
                blob: "opaque".into(),
                dialect: "responses".into(),
            },
            text: Some("visible fallback is not permission to drop state".into()),
        }],
    }])
    .unwrap();
    cases.push(("opaque provider content".into(), provider_content));
    let mut audio = request();
    let wav = include_bytes!("../../../ego-core/tests/fixtures/audio/pcm16-mono.wav");
    audio.messages = serde_json::from_value(json!([{"role":"user", "content":[
        {"type":"audio", "audio":{"bytes":wav.as_slice(), "media_type":"wav"}}
    ]}]))
    .unwrap();
    cases.push(("audio input".into(), audio));
    let mut image = request();
    // Existing one-pixel input used by the image projection tests; not a service response.
    let png = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap();
    image.messages = serde_json::from_value(json!([{"role":"user", "content":[
        {"type":"image", "image":{"bytes":png, "media_type":"png", "width":1, "height":1}}
    ]}]))
    .unwrap();
    cases.push(("image input".into(), image));
    let accepted: Vec<_> = cases
        .into_iter()
        .filter_map(|(name, req)| {
            ChatMaxTokens
                .build_request(&target, &req)
                .is_ok()
                .then_some(name)
        })
        .collect();
    assert!(
        accepted.is_empty(),
        "silently accepted unsupported requests: {accepted:?}"
    );
}

// Catches losing tools, historical arguments/results, or rejecting production's None cache marker.
#[test]
fn declared_tools_and_continuation_preserve_exact_request_subset() {
    let mut req = request();
    req.tools = vec![ToolDescriptor {
        name: "lookup".into(),
        description: "Look up a value".into(),
        parameters: json!({"type":"object"}),
    }];
    req.cache = Some(PromptCache {
        verifier: None,
        session_id: ego_core::event::id::SessionId::new(),
        lineage_root: ego_core::event::id::SessionId::new(),
        retention: CacheRetention::None,
        stable_messages: 1,
    });
    req.messages = Conversation::new(vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Look up a".into(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                call_id: "call_1".into(),
                name: "lookup".into(),
                args: "{\"key\":\"a\"}".into(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                call_id: "call_1".into(),
                output: "found".into(),
                is_error: false,
            }],
        },
    ])
    .unwrap();
    assert_eq!(
        body(&req),
        json!({
            "model":"deployed-nonreasoning-model", "stream":true, "max_tokens":19,
            "tools":[{"type":"function", "function":{"name":"lookup", "description":"Look up a value", "parameters":{"type":"object"}}}],
            "messages":[
                {"role":"user", "content":"Look up a"},
                {"role":"assistant", "tool_calls":[{"id":"call_1", "type":"function", "function":{"name":"lookup", "arguments":"{\"key\":\"a\"}"}}]},
                {"role":"tool", "content":"found", "tool_call_id":"call_1"}
            ]
        })
    );
}

fn binding(contract: ProviderModelContract) -> ProviderInstanceModel {
    ProviderInstanceModel {
        upstream_id: "deployment/actual-model".into(),
        contract,
        metering: ego_core::budget::MeteringIdentity::Unmetered {
            revision: ego_core::cost::MeteringRevision("operator/local-test".into()),
        },
        facts: ModelCapabilities {
            schema_version: ModelCapabilities::SCHEMA_VERSION,
            max_context_tokens: Some(8192),
            max_output_tokens: Some(1024),
            max_tools: Some(8),
            supports_parallel_tool_calls: false,
            reasoning_carryover: ReasoningCarryover::Unsupported,
            reasoning_efforts: ReasoningEffortSupport::Only([ThinkingLevel::Off].into()),
            default_reasoning_effort: Some(ThinkingLevel::Off),
            reasoning_state_dialect: None,
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::None,
            transport: Transport::Sse,
            image_input: ImageInputSupport::Unsupported,
            audio_input: AudioInputSupport::Unsupported,
        },
    }
}

// Catches selecting wire from alias spelling, or static/runtime resolver disagreement.
#[test]
fn configured_resolution_uses_explicit_contract_and_upstream_id_in_both_paths() {
    let providers = ResolvedProviders {
        instances: BTreeMap::from([(
            "local".parse().unwrap(),
            Sourced {
                source: Source::User,
                value: ProviderInstance {
                    profile: ProviderProfile::OpenaiCompatible,
                    base_url: "http://127.0.0.1:1234/prefix/v1/".parse().unwrap(),
                    credential_ref: None,
                    models: BTreeMap::from([
                        (
                            "gpt-name-is-not-a-contract".parse().unwrap(),
                            binding(ProviderModelContract::ChatMaxTokens),
                        ),
                        (
                            "max-tokens-is-not-a-contract".parse().unwrap(),
                            binding(ProviderModelContract::ChatStandard),
                        ),
                    ]),
                },
            },
        )]),
    };
    let catalogue = Catalogue::new([ProviderRow::openai()])
        .with_instances(&providers, &BTreeMap::new())
        .unwrap();
    let (http, unasked) = crate::support::unasked();
    for (alias, legacy) in [
        ("gpt-name-is-not-a-contract", true),
        ("max-tokens-is-not-a-contract", false),
    ] {
        let slug = ModelSlug::parse(&format!("local/{alias}")).unwrap();
        let profile = catalogue.static_request_profile(&slug).unwrap().unwrap();
        let resolved = futures::executor::block_on(catalogue.resolve(
            &slug,
            &http,
            &crate::support::credential(),
        ))
        .unwrap();
        assert_eq!(profile.request_model(), "deployment/actual-model");
        assert_eq!(resolved.model, "deployment/actual-model");
        let mut req = request();
        req.model = resolved.model.clone();
        profile.ensure_expressible(&req).unwrap();
        let base = resolved.row.base();
        let wire = resolved
            .wire
            .build_request(&RequestTarget::new(base, &[]), &req)
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(wire.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            value[if legacy {
                "max_tokens"
            } else {
                "max_completion_tokens"
            }],
            19
        );
        assert!(
            value
                .get(if legacy {
                    "max_completion_tokens"
                } else {
                    "max_tokens"
                })
                .is_none()
        );
        req.thinking = ThinkingLevel::High;
        assert_eq!(profile.ensure_expressible(&req).is_err(), legacy);
        assert_eq!(
            resolved
                .wire
                .build_request(&RequestTarget::new(base, &[]), &req)
                .is_err(),
            legacy
        );
    }
    assert!(
        unasked.asked().is_empty(),
        "configured resolution must not discover or dispatch"
    );
}

// Catches admitting a nonreasoning contract with facts that promise unsupported controls.
#[test]
fn contract_admission_refuses_contradictory_model_facts() {
    assert!(ProviderProfile::OpenaiCompatible.allows(ProviderModelContract::ChatMaxTokens));
    assert!(!ProviderProfile::Openai.allows(ProviderModelContract::ChatMaxTokens));
    let valid = binding(ProviderModelContract::ChatMaxTokens);
    let encoded = serde_json::to_value(&valid).unwrap();
    assert_eq!(encoded["contract"], "chat-max-tokens");
    assert_eq!(
        serde_json::from_value::<ProviderInstanceModel>(encoded).unwrap(),
        valid
    );
    valid.validate().unwrap();
    let mut text_only = valid.clone();
    text_only.facts.max_tools = Some(0);
    text_only.validate().unwrap();
    let mut invalid = Vec::new();
    let mut model = valid.clone();
    model.facts.reasoning_efforts = ReasoningEffortSupport::Any;
    invalid.push(model);
    let mut model = valid.clone();
    model.facts.default_reasoning_effort = None;
    invalid.push(model);
    let mut model = valid.clone();
    model.facts.reasoning_carryover = ReasoningCarryover::SameProviderOnly;
    invalid.push(model);
    let mut model = valid.clone();
    model.facts.reasoning_state_dialect = Some("foreign".into());
    invalid.push(model);
    let mut model = valid.clone();
    model.facts.supports_parallel_tool_calls = true;
    invalid.push(model);
    let mut model = valid.clone();
    model.facts.max_tools = None;
    invalid.push(model);
    let mut model = valid.clone();
    model.facts.cache = CacheRetention::Short;
    invalid.push(model);
    let mut model = valid.clone();
    model.facts.transport = Transport::WebSocket;
    invalid.push(model);
    let mut model = valid;
    model.facts.audio_input = AudioInputSupport::SupportedWav;
    invalid.push(model);
    let accepted: Vec<_> = invalid
        .iter()
        .enumerate()
        .filter_map(|(index, model)| model.validate().is_ok().then_some(index))
        .collect();
    assert!(
        accepted.is_empty(),
        "contradictory facts accepted: {accepted:?}"
    );
}
