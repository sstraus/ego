//! Exact GLM-5.3 and Kimi K3.1 offline contracts. Local schema-derived
//! projections reuse the existing Chat decoder fixture, NOT provider captures.

use ego_ai::http::{HttpError, HttpResponse};
use ego_ai::wire::kimi_api::{self, KIMI_API_REASONING_DIALECT, KimiApi};
use ego_ai::wire::zai::{self, ZAI_REASONING_DIALECT, Zai};
use ego_ai::wire::{RequestRefused, WireFormat};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::history::conversation_from;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, ErrorKind, GenRequest, Message, ReasoningState, Role, StopReason,
    ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::{HttpHeader, ToolResult};
use ego_core::state::SessionState;
use ego_core::store::codec::{StoreLine, decode_line, encode_record};
use ego_core::store::session_store::SessionEntry;
use futures::StreamExt;
use serde_json::{Value, json};

use ego_ai::catalogue::{Catalogue, HostedModel, ModelSlug, SuppliedAuthority, tabulated};
use ego_ai::generator::WireGenerator;
use ego_ai::provider::{Credentials, MissingCredential, ProviderRow, Secret};
use ego_core::budget::BudgetGate;
use ego_core::generator::attempt::GenerationAttempt;
use ego_core::generator::capabilities::{
    AudioInputSupport, CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities,
    ReasoningCarryover, ReasoningEffortSupport, Transport,
};
use ego_core::generator::port::Generator;
use std::sync::Mutex;

fn profiles() -> [(
    ProviderRow,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
); 2] {
    [
        (
            ProviderRow::zai_default(),
            "ZAI_API_KEY",
            "glm-5.3",
            "https://api.z.ai/api/paas/v4/",
            "/api/paas/v4/chat/completions",
        ),
        (
            ProviderRow::kimi_api_default(),
            "MOONSHOT_API_KEY",
            "kimi-k3.1-pro",
            "https://api.moonshot.ai/v1/",
            "/v1/chat/completions",
        ),
    ]
}

struct ScopedKey {
    expected: &'static str,
    present: bool,
    asked: Mutex<Vec<String>>,
}
impl Credentials for ScopedKey {
    fn secret(&self, var: &str) -> Result<Secret, MissingCredential> {
        self.asked.lock().unwrap().push(var.into());
        // On the missing-key path every OTHER source is available. This
        // catches a fallback to the sibling API key or subscription store.
        if (var == self.expected) == self.present {
            Ok("offline-profile-key".into())
        } else {
            Err(MissingCredential { var: var.into() })
        }
    }
}

// Explicit operator fixture facts, NOT a published provider context claim.
fn profile_caps(dialect: &str) -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        max_context_tokens: Some(8192),
        max_output_tokens: Some(4096),
        max_tools: Some(8),
        supports_parallel_tool_calls: false,
        reasoning_carryover: ReasoningCarryover::SameProviderOnly,
        reasoning_efforts: ReasoningEffortSupport::Only(
            [ThinkingLevel::Low, ThinkingLevel::High, ThinkingLevel::Max].into(),
        ),
        default_reasoning_effort: Some(ThinkingLevel::Max),
        reasoning_state_dialect: Some(dialect.into()),
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::None,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unsupported,
        audio_input: AudioInputSupport::Unsupported,
    }
}

/// Catches subscription/sibling fallback or credential-dependent row construction.
#[test]
fn api_key_profiles_report_their_own_missing_key_without_fallback() {
    let [zai, kimi] = profiles().map(|(row, key, _, base, _)| (row, key, base));
    let mistral = (
        ProviderRow::mistral_default(),
        "MISTRAL_API_KEY",
        "https://api.mistral.ai/v1/",
    );
    for (row, key, base) in [zai, kimi, mistral] {
        assert_eq!(row.base().as_str(), base);
        let creds = ScopedKey {
            expected: key,
            present: false,
            asked: Mutex::default(),
        };
        assert_eq!(row.credential.resolve(&creds).unwrap_err().var, key);
        assert_eq!(*creds.asked.lock().unwrap(), [key]);
    }
}

/// Catches implicit default-model facts/discovery, wrong row-selected wire or
/// path, and flattening a bearer secret before the guarded generation boundary.
#[test]
fn api_key_profiles_resolve_explicit_facts_and_dispatch_exact_guarded_routes() {
    futures::executor::block_on(async {
        for (row, key, model, base, path) in profiles() {
            assert_eq!(row.base().as_str(), base);
            let dialect = if row.id() == "zai" {
                ZAI_REASONING_DIALECT
            } else {
                KIMI_API_REASONING_DIALECT
            };
            let creds = ScopedKey {
                expected: key,
                present: true,
                asked: Mutex::default(),
            };
            let credential = row.credential.resolve(&creds).unwrap();
            assert!(credential.base_override().is_none());
            let headers = credential.headers();
            assert_eq!(headers.len(), 1);
            assert_eq!(headers[0].name(), "authorization");
            assert!(headers[0].is_secret());
            assert!(!format!("{credential:?}").contains("offline-profile-key"));
            let slug = ModelSlug::parse(&format!("{}/{model}", row.id())).unwrap();
            let catalogue = Catalogue::new([row]);
            let (unasked, no_calls) = crate::support::unasked();
            assert!(
                catalogue
                    .resolve(&slug, &unasked, &credential)
                    .await
                    .is_err()
            );
            assert!(no_calls.asked().is_empty(), "no implicit discovery");
            let caps = profile_caps(dialect);
            let catalogue = catalogue.with_hosted(
                &slug.to_string(),
                HostedModel {
                    authority: SuppliedAuthority::System,
                    capabilities: caps.clone(),
                    metering: tabulated(),
                    rates: None,
                },
            );
            let resolved = catalogue
                .resolve(&slug, &unasked, &credential)
                .await
                .unwrap();
            assert!(no_calls.asked().is_empty());
            assert_eq!(resolved.capabilities, caps);
            assert!(resolved.rates.is_none());
            assert_eq!(resolved.metering, tabulated());
            assert!(resolved.row.cost.is_none());
            let (http, record) = crate::support::server([(
                200,
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )]);
            let mut req = request(model);
            req.thinking = resolved.capabilities.default_reasoning_effort.unwrap();
            let mut gate = BudgetGate::unlimited(ego_core::cost::CurrencyCode::new("USD").unwrap());
            let admission = gate.admit(&resolved.metering, None, &req, &caps).unwrap();
            let attempt = GenerationAttempt::new(
                ego_core::event::id::GenerationId::new(),
                admission,
                &resolved.metering,
                None,
                &req,
                &caps,
            )
            .unwrap();
            let generator = WireGenerator::new(resolved.row, resolved.wire, http, credential);
            let events: Vec<_> = generator.generate(req, attempt).collect().await;
            assert!(
                events.contains(&AiEvent::Stop(StopReason::Stop)),
                "{events:?}"
            );
            let asked = record.asked();
            assert_eq!(asked.len(), 1);
            assert_eq!(asked[0].path, path);
            assert_eq!(asked[0].method, ego_ai::http::Method::Post);
            assert!(
                asked[0]
                    .headers
                    .iter()
                    .any(|(name, value)| name == "authorization"
                        && value == "Bearer offline-profile-key")
            );
            let value: Value = serde_json::from_str(asked[0].body.as_ref().unwrap()).unwrap();
            assert_eq!(value["model"], model);
            assert_eq!(value["reasoning_effort"], "max");
            if model == "glm-5.3" {
                assert_eq!(value["thinking"]["clear_thinking"], false);
                assert_eq!(value["max_tokens"], 4096);
                assert!(value.get("max_completion_tokens").is_none());
            } else {
                assert!(value.get("thinking").is_none());
                assert_eq!(value["max_completion_tokens"], 4096);
                assert!(value.get("max_tokens").is_none());
            }
            assert_eq!(*creds.asked.lock().unwrap(), [key], "one explicit key read");
        }
    });
}

/// Catches configured aliases retaining the profile's default key/base or a
/// generic wire, and static preflight disagreeing with runtime model resolution.
#[test]
fn configured_reasoning_instances_bind_exact_static_and_runtime_profiles() {
    use ego_core::config::{
        ConnectionAccess, ConnectionConfig, ProviderInstance, ProviderInstanceModel,
        ProviderModelContract, ProviderProfile, ResolvedProviders, Source, Sourced,
    };
    use std::collections::BTreeMap;
    futures::executor::block_on(async {
        let [zai, kimi] = profiles();
        let mistral = (
            ProviderRow::mistral_default(),
            "",
            "configured-mistral",
            "",
            "",
        );
        for ((row, _, model, _, _), responses) in [
            (zai, false),
            (kimi.clone(), false),
            (kimi, true),
            (mistral, false),
        ] {
            let (profile, contract, dialect) = if row.id() == "mistral" {
                (
                    ProviderProfile::Mistral,
                    ProviderModelContract::MistralConversations,
                    ego_ai::wire::mistral_conversations::DIALECT,
                )
            } else if responses {
                (
                    ProviderProfile::KimiApi,
                    ProviderModelContract::KimiResponses,
                    ego_ai::wire::kimi_responses::DIALECT,
                )
            } else if row.id() == "zai" {
                (
                    ProviderProfile::Zai,
                    ProviderModelContract::Zai,
                    ZAI_REASONING_DIALECT,
                )
            } else {
                (
                    ProviderProfile::KimiApi,
                    ProviderModelContract::KimiApi,
                    KIMI_API_REASONING_DIALECT,
                )
            };
            let mut caps = profile_caps(dialect);
            let base = if responses {
                caps.cache = CacheRetention::Long;
                ego_ai::provider::KIMI_API_BASE
            } else if profile == ProviderProfile::Mistral {
                ego_ai::wire::mistral_conversations::BASE
            } else {
                "https://inference.example/work/v1/"
            };
            let providers = ResolvedProviders {
                instances: BTreeMap::from([(
                    "reasoning-work".parse().unwrap(),
                    Sourced {
                        source: Source::User,
                        value: ProviderInstance {
                            profile,
                            base_url: base.parse().unwrap(),
                            credential_ref: Some("work-key".parse().unwrap()),
                            models: BTreeMap::from([(
                                "assistant".parse().unwrap(),
                                ProviderInstanceModel {
                                    upstream_id: model.into(),
                                    contract,
                                    facts: caps.clone(),
                                    metering: ego_core::budget::MeteringIdentity::Currency {
                                        settlement: ego_core::budget::SettlementSource::Quote,
                                    },
                                },
                            )]),
                        },
                    },
                )]),
            };
            let connections = BTreeMap::from([(
                "work-key".into(),
                ConnectionConfig {
                    name: "Work key".into(),
                    vendor: row.id().into(),
                    access: ConnectionAccess::Key,
                    key_env: Some("WORK_API_KEY".into()),
                    azure_resource: None,
                },
            )]);
            let catalogue = Catalogue::new([row])
                .with_instances(&providers, &connections)
                .unwrap();
            let slug = ModelSlug::parse("reasoning-work/assistant").unwrap();
            let static_profile = catalogue.static_request_profile(&slug).unwrap().unwrap();
            assert_eq!(static_profile.request_model(), model);
            assert_eq!(static_profile.capabilities(), &caps);
            let mut req = request(static_profile.request_model());
            req.thinking = static_profile
                .capabilities()
                .default_reasoning_effort
                .unwrap();
            assert!(static_profile.ensure_expressible(&req).is_ok());
            assert!(
                static_profile
                    .capabilities()
                    .resolve_reasoning_effort(Some(ThinkingLevel::Off))
                    .is_err()
            );
            if profile != ProviderProfile::Mistral {
                let mut unsupported = req.clone();
                unsupported.thinking = ThinkingLevel::Off;
                assert!(static_profile.ensure_expressible(&unsupported).is_err());
            }
            let creds = ScopedKey {
                expected: "WORK_API_KEY",
                present: true,
                asked: Mutex::default(),
            };
            let row = catalogue.row("reasoning-work").unwrap();
            assert_eq!(row.base().as_str(), base);
            let credential = row.credential.resolve(&creds).unwrap();
            assert!(credential.headers()[0].is_secret());
            let mistral = profile == ProviderProfile::Mistral;
            if responses || mistral {
                req.web_search = Some(ego_core::generator::types::HostedSearch {
                    max_requests: std::num::NonZeroU32::new(5).unwrap(),
                    allow_live_access: true,
                });
            }
            let fixture = if mistral {
                "data: {\"type\":\"conversation.response.done\",\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n"
            } else if responses {
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
            } else {
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
            };
            let (http, record) = crate::support::server([(200, fixture)]);
            let resolved = catalogue.resolve(&slug, &http, &credential).await.unwrap();
            assert!(
                record.asked().is_empty(),
                "configured resolution must not discover"
            );
            assert_eq!(resolved.model, model);
            assert_eq!(&resolved.capabilities, static_profile.capabilities());
            let mut gate = BudgetGate::unlimited(ego_core::cost::CurrencyCode::new("USD").unwrap());
            let admission = gate.admit(&resolved.metering, None, &req, &caps).unwrap();
            let attempt = GenerationAttempt::new(
                ego_core::event::id::GenerationId::new(),
                admission,
                &resolved.metering,
                None,
                &req,
                &caps,
            )
            .unwrap();
            let generator = WireGenerator::new(resolved.row, resolved.wire, http, credential);
            let events: Vec<_> = generator.generate(req, attempt).collect().await;
            assert!(
                events.contains(&AiEvent::Stop(StopReason::Stop)),
                "{events:?}"
            );
            let asked = record.asked();
            assert_eq!(asked.len(), 1);
            let value: Value = serde_json::from_str(asked[0].body.as_ref().unwrap()).unwrap();
            assert_eq!(value["model"], model, "never send the local alias");
            assert_eq!(*creds.asked.lock().unwrap(), ["WORK_API_KEY"]);
            if mistral {
                assert_eq!(asked[0].path, "/v1/conversations");
                assert_eq!(value["store"], false);
                assert_eq!(value["completion_args"]["reasoning_effort"], "max");
                assert_eq!(value["tools"][0]["function"]["name"], "get_weather");
                assert_eq!(value["tools"][1], json!({"type":"web_search"}));
                continue;
            }
            if responses {
                assert_eq!(asked[0].path, "/v1/responses");
                assert_eq!(value["reasoning"]["effort"], "max");
                assert_eq!(
                    value["tools"],
                    json!([
                        {"type":"function", "name":"get_weather", "description":"Get weather",
                         "parameters":{"type":"object", "properties":{}}},
                        {"type":"web_search"}
                    ])
                );
                continue;
            }
            assert_eq!(asked[0].path, "/work/v1/chat/completions");
            assert_eq!(value["reasoning_effort"], "max");
            assert_eq!(
                value.get("thinking").is_some(),
                profile == ProviderProfile::Zai
            );
            assert_eq!(
                value.get("max_tokens").is_some(),
                profile == ProviderProfile::Zai
            );
            assert_eq!(
                value.get("max_completion_tokens").is_some(),
                profile == ProviderProfile::KimiApi
            );
        }
    });
}

const TOOL_STREAM: &str = include_str!("../goldens/profile/deepseek/stream/tool_calls.sse");
const EXPECTED_REASONING: &str = "Today is 2026-04-19, so tomorrow is 2026-04-20. Now I'll call the weather function for Hangzhou.";
const CALL_ID: &str = "call_00_H2SCW6136vWJGq9SQlBuhVt4";
const CALL_ARGS: &str = r#"{"location": "Hangzhou", "date": "2026-04-20"}"#;
type Body = fn(&GenRequest) -> Result<Vec<u8>, RequestRefused>;

fn mappings() -> [(&'static dyn WireFormat, Body, &'static str, &'static str); 2] {
    [
        (&Zai, zai::request_body, ZAI_REASONING_DIALECT, "glm-5.3"),
        (
            &KimiApi,
            kimi_api::request_body,
            KIMI_API_REASONING_DIALECT,
            "kimi-k3.1-pro",
        ),
    ]
}

fn request(model: &str) -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: model.into(),
        messages: Conversation::new(vec![Message::text(Role::User, "Weather?")]).unwrap(),
        tools: vec![ToolDescriptor {
            name: "get_weather".into(),
            description: "Get weather".into(),
            parameters: json!({"type":"object","properties":{}}),
        }],
        thinking: ThinkingLevel::High,
        max_output_tokens: Some(4096),
        parallel_tool_calls: false,
    }
}

fn body(mapping: Body, request: &GenRequest) -> Value {
    serde_json::from_slice(&mapping(request).expect("exact request is expressible")).unwrap()
}

fn response(status: u16, transcript: &str) -> Result<HttpResponse, HttpError> {
    let chunk_size = if transcript.len() > 8192 { 8192 } else { 7 };
    let chunks: Vec<_> = transcript
        .as_bytes()
        .chunks(chunk_size)
        .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
        .collect();
    Ok(HttpResponse {
        status,
        headers: vec![HttpHeader::new("content-type", b"text/event-stream".to_vec()).unwrap()],
        body: futures::stream::iter(chunks).boxed(),
    })
}

fn events(mapping: &dyn WireFormat, transcript: &str) -> Vec<AiEvent> {
    futures::executor::block_on(mapping.decode(response(200, transcript), None).collect())
}

fn reasoning(dialect: &str, blob: &str) -> ContentBlock {
    ContentBlock::Reasoning {
        state: ReasoningState {
            connection: None,
            dialect: dialect.into(),
            blob: blob.into(),
        },
    }
}

/// Catches wrong ceiling names, a missing preservation switch, inherited
/// undocumented streaming/parallel controls, and obsolete K2 thinking fields.
#[test]
fn exact_request_controls_and_ceilings_are_not_inherited_from_chat_compatibility() {
    let zai = body(zai::request_body, &request("glm-5.3"));
    assert_eq!(
        zai,
        json!({
            "model":"glm-5.3", "messages":[{"role":"user","content":"Weather?"}],
            "stream":true, "reasoning_effort":"high", "max_tokens":4096,
            "thinking":{"type":"enabled","clear_thinking":false}, "tool_stream":true,
            "tools":[{"type":"function","function":{"name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{}}}}]
        })
    );
    let kimi = body(kimi_api::request_body, &request("kimi-k3.1-pro"));
    assert_eq!(
        kimi,
        json!({
            "model":"kimi-k3.1-pro", "messages":[{"role":"user","content":"Weather?"}],
            "stream":true, "stream_options":{"include_usage":true},
            "reasoning_effort":"high", "max_completion_tokens":4096,
            "tools":[{"type":"function","function":{"name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{}}}}]
        })
    );
}

/// Catches requested efforts silently replaced by defaults and invalid caps
/// sent to the server; accepted levels must retain their exact spelling.
#[test]
fn always_reasoning_models_refuse_unserved_efforts_and_out_of_range_caps() {
    for (_, mapping, _, model) in mappings() {
        for level in [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Medium,
            ThinkingLevel::XHigh,
        ] {
            let mut req = request(model);
            req.thinking = level;
            assert!(mapping(&req).is_err(), "{model}: {level:?}");
        }
        for (level, spelling) in [
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::High, "high"),
            (ThinkingLevel::Max, "max"),
        ] {
            let mut req = request(model);
            req.thinking = level;
            assert_eq!(body(mapping, &req)["reasoning_effort"], spelling);
        }
        let cap = if model == "glm-5.3" {
            131_072
        } else {
            1_048_576
        };
        for invalid in [0, cap + 1, u64::MAX] {
            let mut req = request(model);
            req.max_output_tokens = Some(invalid);
            assert!(mapping(&req).is_err(), "{model}: {invalid}");
        }
        let mut req = request(model);
        req.max_output_tokens = Some(cap);
        assert!(mapping(&req).is_ok());
        req.max_output_tokens = None;
        let value = body(mapping, &req);
        assert!(value.get("max_tokens").is_none());
        assert!(value.get("max_completion_tokens").is_none());
    }
}

/// Catches silently lost controls at the exact mapping boundary, even when a
/// caller bypasses catalogue capability checks.
#[test]
fn unimplemented_parallel_search_and_cache_controls_refuse_before_dispatch() {
    for (wire, mapping, dialect, model) in mappings() {
        let mut req = request(model);
        req.parallel_tool_calls = true;
        assert!(mapping(&req).is_err());
        let mut req = request(model);
        req.web_search = Some(hosted_search());
        // Z.ai encodes the documented in-chat tool; Kimi API has none here.
        let searches = dialect == ZAI_REASONING_DIALECT;
        assert_eq!(mapping(&req).is_ok(), searches);
        assert_eq!(wire.supports_web_search(), searches);
        let mut req = request(model);
        req.cache = Some(ego_core::generator::types::PromptCache {
            verifier: None,
            session_id: ego_core::event::id::SessionId::new(),
            lineage_root: ego_core::event::id::SessionId::new(),
            retention: ego_core::generator::capabilities::CacheRetention::Short,
            stable_messages: 1,
        });
        assert!(mapping(&req).is_err());
    }
}

fn hosted_search() -> ego_core::generator::types::HostedSearch {
    ego_core::generator::types::HostedSearch {
        max_requests: std::num::NonZeroU32::new(1).unwrap(),
        allow_live_access: false,
    }
}

/// Catches a guide/standalone engine spelling instead of the Chat schema's,
/// the hosted-search slot leaking as a callable local function, a reordered
/// function prefix, `tool_stream` sent with no function to stream, and an
/// invented search counter or result-cache field in the request.
#[test]
fn zai_hosted_search_sends_only_the_documented_chat_tool() {
    let native =
        json!({"type":"web_search","web_search":{"enable":true,"search_engine":"search_pro_jina"}});
    let weather = json!({"type":"function","function":{"name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{}}}});
    let slot = ToolDescriptor {
        name: "web_search".into(),
        description: "Hosted search".into(),
        parameters: json!({"type":"object"}),
    };
    let mut req = request("glm-5.3");
    req.tools.push(slot.clone());
    let plain = body(zai::request_body, &req);
    req.web_search = Some(hosted_search());
    let searching = body(zai::request_body, &req);
    assert_eq!(searching["tools"], json!([weather, native]));
    assert_eq!(searching["tool_stream"], true);
    assert_eq!(searching["stream"], true);
    // Without hosted search the same slot name stays an ordinary function.
    assert_eq!(plain["tools"][1]["function"]["name"], "web_search");
    // Everything but the tool list is the ordinary request, field for field.
    let (mut plain, mut searching) = (plain, searching);
    plain.as_object_mut().unwrap().remove("tools");
    searching.as_object_mut().unwrap().remove("tools");
    assert_eq!(plain, searching);

    req.tools = vec![slot];
    let search_only = body(zai::request_body, &req);
    assert_eq!(search_only["tools"], json!([native]));
    assert_eq!(search_only["tool_stream"], false);
    req.tools.clear();
    assert_eq!(body(zai::request_body, &req)["tools"], json!([native]));
}

/// Catches the engine's always-present disabled cache bookkeeping being
/// mistaken for an enabled explicit cache control.
#[test]
fn production_disabled_cache_metadata_does_not_block_or_change_native_requests() {
    use ego_core::generator::capabilities::CacheRetention;
    for (_, mapping, _, model) in mappings() {
        let mut req = request(model);
        let absent = body(mapping, &req);
        req.cache = Some(ego_core::generator::types::PromptCache {
            verifier: None,
            session_id: ego_core::event::id::SessionId::new(),
            lineage_root: ego_core::event::id::SessionId::new(),
            retention: CacheRetention::None,
            stable_messages: 1,
        });
        assert_eq!(body(mapping, &req), absent);
        for retention in [CacheRetention::Short, CacheRetention::Long] {
            req.cache.as_mut().unwrap().retention = retention;
            assert!(mapping(&req).is_err());
        }
    }
}

/// Catches modality loss and opaque provider-content loss when the mapping
/// is called directly instead of through catalogue admission.
#[test]
fn unsupported_modalities_and_provider_items_are_not_silently_projected() {
    let gif = [
        71, 73, 70, 56, 57, 97, 1, 0, 1, 0, 128, 0, 0, 255, 255, 255, 0, 0, 0, 33, 249, 4, 1, 0, 0,
        0, 0, 44, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 68, 1, 0, 59,
    ];
    let wav = [
        82, 73, 70, 70, 38, 0, 0, 0, 87, 65, 86, 69, 102, 109, 116, 32, 16, 0, 0, 0, 1, 0, 1, 0,
        64, 31, 0, 0, 128, 62, 0, 0, 2, 0, 16, 0, 100, 97, 116, 97, 2, 0, 0, 0, 0, 0,
    ];
    let image: ContentBlock = serde_json::from_value(
        json!({"type":"image","image":{"bytes":gif.as_slice(),"media_type":"gif","width":1,"height":1}}),
    )
    .unwrap();
    let audio: ContentBlock = serde_json::from_value(
        json!({"type":"audio","audio":{"bytes":wav.as_slice(),"media_type":"wav"}}),
    )
    .unwrap();
    for (_, mapping, dialect, model) in mappings() {
        for block in [image.clone(), audio.clone()] {
            let mut req = request(model);
            req.messages = Conversation::new(vec![Message {
                role: Role::User,
                content: vec![block],
            }])
            .unwrap();
            assert!(mapping(&req).is_err());
        }
        let mut req = request(model);
        req.messages = Conversation::new(vec![
            Message::text(Role::User, "First"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ProviderContent {
                    state: ReasoningState {
                        connection: None,
                        dialect: dialect.into(),
                        blob: "opaque item".into(),
                    },
                    text: None,
                }],
            },
            Message::text(Role::User, "Next"),
        ])
        .unwrap();
        assert!(mapping(&req).is_err());
    }
}

/// Catches filtering replay by currently declared tools, or replaying a
/// sibling dialect into a nominally compatible endpoint.
#[test]
fn full_ordered_history_replays_even_without_current_tool_declarations() {
    for (_, mapping, dialect, model) in mappings() {
        let mut req = request(model);
        req.tools.clear();
        req.messages = Conversation::new(vec![
            Message::text(Role::User, "First"),
            Message {
                role: Role::Assistant,
                content: vec![
                    reasoning(dialect, "first\n完整 "),
                    ContentBlock::Text {
                        text: "Answer one".into(),
                    },
                ],
            },
            Message::text(Role::User, "Next"),
            Message {
                role: Role::Assistant,
                content: vec![
                    reasoning(dialect, " second\t"),
                    ContentBlock::Text {
                        text: "Answer two".into(),
                    },
                ],
            },
            Message::text(Role::User, "Again"),
        ])
        .unwrap();
        let value = body(mapping, &req);
        assert_eq!(value["messages"][1]["reasoning_content"], "first\n完整 ");
        assert_eq!(value["messages"][3]["reasoning_content"], " second\t");
        for other in [
            ZAI_REASONING_DIALECT,
            KIMI_API_REASONING_DIALECT,
            "deepseek-chat",
            "openrouter-chat",
        ] {
            if other == dialect {
                continue;
            }
            let mut incompatible = req.clone();
            incompatible.messages = Conversation::new(vec![
                Message::text(Role::User, "First"),
                Message {
                    role: Role::Assistant,
                    content: vec![reasoning(other, "not this dialect")],
                },
                Message::text(Role::User, "Again"),
            ])
            .unwrap();
            assert!(
                mapping(&incompatible).is_err(),
                "{dialect} admitted {other}"
            );
        }
    }
}

/// Exercise the actual durable record codec/reducer/history boundary, not
/// state-only serde or a copied production projection.
fn reload(kinds: Vec<(ItemId, EventKind)>) -> Conversation {
    let turn = TurnId::new();
    let mut seq = Seq::ZERO;
    let records: Vec<_> = kinds
        .into_iter()
        .map(|(item, kind)| {
            let event = DomainEvent {
                env: Envelope {
                    seq: seq.next(),
                    id: EventId::new(),
                    turn,
                    item: Some(item),
                    parent: None,
                    ts: Timestamp::from_millis(0),
                    producer: Producer::Main,
                },
                kind,
            };
            encode_record(event.env.seq, &SessionEntry::Event(event)).unwrap()
        })
        .collect();
    let mut state = SessionState::default();
    for bytes in records {
        let StoreLine::Record(record) = decode_line(&bytes).unwrap() else {
            panic!("event record")
        };
        let SessionEntry::Event(event) = serde_json::from_str(record.payload.get()).unwrap() else {
            panic!("event entry")
        };
        state.apply(&event);
    }
    let (conversation, repairs) = conversation_from(&state).unwrap();
    assert!(repairs.is_empty());
    conversation
}

/// Catches losing original reasoning/content/tool bytes at persistence, using
/// display extensions as replay, or omitting an empty tool-only content string.
#[test]
fn exact_reasoning_tool_continuation_survives_real_session_record_replay() {
    // Explicit fault injection into an unchanged shared schema fixture. This
    // is not relabeled as a recording of either newly admitted provider.
    let transcript = TOOL_STREAM.replacen(
        r#""reasoning_content":"""#,
        r#""reasoning":"display-only extension","reasoning_content":"""#,
        1,
    );
    for (wire, mapping, dialect, model) in mappings() {
        for answer in ["", "Checking weather now.\n完整"] {
            let transcript = transcript.replace(
                r#""delta":{"content":""}"#,
                &format!(r#""delta":{{"content":{}}}"#, json!(answer)),
            );
            let decoded = events(wire, &transcript);
            assert!(decoded.contains(&AiEvent::ReasoningDelta {
                text: "display-only extension".into()
            }));
            let mut kinds = vec![(
                ItemId::new(),
                EventKind::UserPrompt {
                    text: "Weather?".into(),
                    images: vec![],
                    audio: vec![],
                    queued_prompt: None,
                },
            )];
            let item = ItemId::new();
            kinds.push((
                item,
                EventKind::ReasoningDelta {
                    text: "not replayable display preview".into(),
                },
            ));
            let mut state_count = 0;
            for event in decoded {
                match event {
                    AiEvent::ReasoningState(state) => {
                        assert_eq!(state.blob, EXPECTED_REASONING);
                        assert_eq!(state.dialect, dialect);
                        state_count += 1;
                        kinds.push((item, EventKind::ReasoningState { state }));
                    }
                    AiEvent::TextDelta { text } => {
                        let item = ItemId::new();
                        kinds.push((item, EventKind::AssistantMessageStarted));
                        kinds.push((item, EventKind::AssistantMessage { text }));
                    }
                    AiEvent::ToolCall {
                        call_id,
                        name,
                        args,
                    } => {
                        assert_eq!(call_id, CALL_ID);
                        assert_eq!(args, CALL_ARGS);
                        let item = ItemId::new();
                        kinds.push((
                            item,
                            EventKind::ToolCallStarted {
                                name,
                                args,
                                call_id: Some(call_id),
                            },
                        ));
                        kinds.push((
                            item,
                            EventKind::ToolCallCompleted {
                                ok: true,
                                result: ToolResult::builtin("Cloudy 7~13°C"),
                            },
                        ));
                    }
                    _ => {}
                }
            }
            assert_eq!(state_count, 1);
            let mut req = request(model);
            req.messages = reload(kinds);
            let value = body(mapping, &req);
            assert_eq!(
                value["messages"],
                json!([
                    {"role":"user","content":"Weather?"},
                    {"role":"assistant","content":answer,"reasoning_content":EXPECTED_REASONING,
                        "tool_calls":[{"id":CALL_ID,"type":"function","function":{"name":"get_weather","arguments":CALL_ARGS}}]},
                    {"role":"tool","tool_call_id":CALL_ID,"content":"Cloudy 7~13°C"}
                ])
            );
            assert!(ego_ai::wire::deepseek::request_body(&req).is_err());
            assert!(ego_ai::wire::openai_completions::request_body(&req).is_err());
            assert!(ego_ai::wire::openai_completions::openrouter_request_body(&req).is_err());
        }
    }
}

/// Catches display-only state synthesis, truncated reasoning sealed into
/// durable state, or losing the existing shared usage/tool assembly contract.
#[test]
fn display_only_and_interrupted_streams_mint_no_native_replay_state() {
    for (wire, _, dialect, _) in mappings() {
        let decoded = events(wire, TOOL_STREAM);
        assert!(decoded.contains(&AiEvent::ReasoningState(ReasoningState {
            connection: None,
            dialect: dialect.into(),
            blob: EXPECTED_REASONING.into(),
        })));
        let usages: Vec<_> = decoded
            .iter()
            .filter_map(|event| match event {
                AiEvent::Usage(usage) => Some(usage),
                _ => None,
            })
            .collect();
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].cache_read_tokens, 256);
        assert_eq!(usages[0].uncached_input_tokens, 56);
        assert_eq!(usages[0].output_tokens, 41);
        assert_eq!(decoded.last(), Some(&AiEvent::Stop(StopReason::ToolUse)));
        let display_only = TOOL_STREAM.replace("reasoning_content", "reasoning");
        assert!(
            !events(wire, &display_only)
                .iter()
                .any(|event| matches!(event, AiEvent::ReasoningState(_)))
        );
        let cut = TOOL_STREAM.find(r#""finish_reason":"tool_calls""#).unwrap();
        let decoded = events(wire, &TOOL_STREAM[..cut]);
        assert!(
            !decoded.iter().any(|event| matches!(
                event,
                AiEvent::ReasoningState(_) | AiEvent::ToolCall { .. }
            ))
        );
        assert_eq!(decoded.last(), Some(&AiEvent::Stop(StopReason::Error)));
    }
}

/// Catches shared Chat sealing state or emitting half-assembled calls before
/// discovering a native failure reason; valid usage must still be preserved.
#[test]
fn zai_failure_terminals_never_mint_state_or_executable_calls() {
    for (reason, expected) in [
        ("network_error", ErrorKind::Transient),
        ("model_context_window_exceeded", ErrorKind::ContextOverflow),
        ("sensitive", ErrorKind::Refused),
        ("unknown_reason", ErrorKind::Unclassified),
    ] {
        let transcript = TOOL_STREAM.replace(
            r#""finish_reason":"tool_calls""#,
            &format!(r#""finish_reason":"{reason}""#),
        );
        let decoded = events(&Zai, &transcript);
        assert!(
            !decoded.iter().any(|event| matches!(
                event,
                AiEvent::ReasoningState(_) | AiEvent::ToolCall { .. }
            )),
            "{decoded:?}"
        );
        assert!(
            decoded
                .iter()
                .any(|event| matches!(event,AiEvent::Error{kind,..} if *kind==expected)),
            "{decoded:?}"
        );
        assert_eq!(
            decoded
                .iter()
                .filter(|event| matches!(event, AiEvent::Stop(_)))
                .count(),
            1
        );
        assert_eq!(decoded.last(), Some(&AiEvent::Stop(StopReason::Error)));
        assert_eq!(
            decoded
                .iter()
                .filter(|event| matches!(event, AiEvent::Usage(_)))
                .count(),
            1
        );
    }
}

/// Catches a skipped malformed native delta later becoming a complete but
/// silently truncated replay payload, and unbounded SSE frame accumulation.
#[test]
fn zai_malformed_or_oversized_frames_cannot_be_sealed_by_a_later_finish() {
    for invalid in [
        "data: {not JSON}\n\n".to_string(),
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":{}}}]}\n\n".to_string(),
        format!("data: {}", "x".repeat(1024 * 1024 + 1)),
    ] {
        let transcript = format!("{invalid}{TOOL_STREAM}");
        let decoded = events(&Zai, &transcript);
        assert!(
            !decoded.iter().any(|event| matches!(
                event,
                AiEvent::ReasoningState(_) | AiEvent::ToolCall { .. }
            )),
            "{decoded:?}"
        );
        assert_eq!(decoded.last(), Some(&AiEvent::Stop(StopReason::Error)));
    }
}

/// Catches flat Z.ai numeric errors losing typed diagnostics, and empty
/// balance code 1113 sharing a retry loop with rate-limit code 1302.
#[test]
fn zai_both_documented_error_envelopes_keep_balance_and_rate_limit_distinct() {
    for (status, code, expected) in [
        (429, 1113, ErrorKind::Refused),
        (429, 1302, ErrorKind::Transient),
        (429, 1308, ErrorKind::Refused),
        (429, 1315, ErrorKind::Refused),
        (400, 1261, ErrorKind::ContextOverflow),
    ] {
        for payload in [
            json!({"code":code,"message":"typed diagnosis","private":"must not leak"}),
            json!({"error":{"code":code.to_string(),"message":"typed diagnosis"},"private":"must not leak"}),
            json!({"code":code,"message":format!("typed diagnosis{}", "x".repeat(60_000)),"private":"must not leak"}),
        ] {
            let decoded: Vec<AiEvent> = futures::executor::block_on(
                Zai.decode(response(status, &payload.to_string()), None)
                    .collect(),
            );
            assert!(decoded.iter().any(|event| matches!(event,AiEvent::Error{message,kind} if *kind==expected && message.contains("typed diagnosis") && message.contains(&code.to_string()) && !message.contains("must not leak"))), "{decoded:?}");
            assert_eq!(decoded.last(), Some(&AiEvent::Stop(StopReason::Error)));
        }
    }
}

/// Catches accidentally routing Kimi's documented nested errors through the
/// flat GLM dialect instead of the shared safe diagnostic/status mechanism.
#[test]
fn kimi_nested_error_preserves_safe_message_and_status_class() {
    let decoded:Vec<AiEvent> = futures::executor::block_on(KimiApi.decode(response(401,
        r#"{"error":{"message":"invalid key","type":"authentication_error","code":"invalid_api_key"},"private":"must not leak"}"#),None).collect());
    assert!(decoded.iter().any(|event| matches!(event,AiEvent::Error{message,kind:ErrorKind::Refused} if message.contains("invalid key") && !message.contains("must not leak"))));
    assert_eq!(decoded.last(), Some(&AiEvent::Stop(StopReason::Error)));
}
