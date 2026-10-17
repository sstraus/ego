//! contract for provider inventory and managed Kimi model selection.
//!
//! regression case deliberately resolves the mapping through the public catalogue
//! seam. The unresolved row is deployment authority; the resolved model is
//! the only value allowed to carry the wire into generation.

use std::sync::Arc;

use base64::Engine as _;
use ego_ai::catalogue::{Catalogue, HostedModel, ModelSlug, SuppliedAuthority, tabulated};
use ego_ai::http::{Http, HttpError, HttpResponse};
use ego_ai::provider::ProviderRow;
use ego_ai::wire::RequestTarget;
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    Conversation, GenRequest, Message, Role, ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::HttpHeader;
use futures::StreamExt;
use futures::executor::block_on;
use serde_json::{Value, json};

mod support;
use support::{Answer, FakeCredentials, StubServer, credential, server};

const KIMI_MODELS: &str = include_str!("goldens/kimi/models/current.json");
const KIMI_OPENAI_REQUEST: &str =
    include_str!("goldens/profile/kimi-coding/request/kimi_openai.json");
const KIMI_ANTHROPIC_REQUEST: &str =
    include_str!("goldens/profile/kimi-coding/request/kimi_anthropic.json");
const KIMI_OPENAI_STREAM: &str = include_str!("goldens/profile/kimi-coding/stream/kimi_openai.sse");
const KIMI_ANTHROPIC_STREAM: &str =
    include_str!("goldens/profile/kimi-coding/stream/kimi_anthropic.sse");
const KIMI_OPENAI_TOOL_STREAM: &str =
    include_str!("goldens/profile/kimi-coding/stream/kimi_openai_tool.sse");
const KIMI_ANTHROPIC_TOOL_STREAM: &str =
    include_str!("goldens/profile/kimi-coding/stream/kimi_anthropic_tool.sse");
const KIMI_OPENAI_ERROR_STREAM: &str =
    include_str!("goldens/profile/kimi-coding/stream/kimi_openai_error.sse");
const KIMI_ANTHROPIC_ERROR_STREAM: &str =
    include_str!("goldens/profile/kimi-coding/stream/kimi_anthropic_error.sse");
const IMAGE_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
const KIMI_OPENAI_IMAGE_REQUEST: &str =
    include_str!("goldens/profile/kimi-coding/request/kimi_openai_image.json");
const KIMI_ANTHROPIC_IMAGE_REQUEST: &str =
    include_str!("goldens/profile/kimi-coding/request/kimi_anthropic_image.json");

fn resolve(
    slug: &str,
    document: impl Into<String>,
) -> (
    Result<ego_ai::catalogue::ResolvedModel, ego_ai::catalogue::ResolveError>,
    Arc<StubServer>,
    ego_ai::provider::ResolvedCredential,
) {
    let catalogue = Catalogue::new([ProviderRow::kimi_coding_default()]);
    let body: &'static str = Box::leak(document.into().into_boxed_str());
    let (http, stub) = server([(200, body)]);
    let credential = credential();
    let result = block_on(catalogue.resolve(
        &ModelSlug::parse(slug).expect("valid Kimi slug"),
        &http,
        &credential,
    ));
    (result, stub, credential)
}

fn supplied_capabilities() -> ego_core::generator::capabilities::ModelCapabilities {
    serde_json::from_value(json!({
        "schema_version":ModelCapabilities::SCHEMA_VERSION,"max_context_tokens":131072,"max_output_tokens":4096,
        "max_tools":16,"supports_parallel_tool_calls":false,
        "reasoning_carryover":"unsupported","reasoning_efforts":"any",
        "reasoning_state_dialect":null,"cancel":"cooperative","cache":"none",
        "transport":"sse","image_input":"unknown"
    }))
    .expect("schema v5 supplied facts")
}

fn request(model: &str, tools: Vec<ToolDescriptor>) -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: model.to_string(),
        messages: Conversation::new(vec![Message::text(Role::User, "say ok")])
            .expect("valid history"),
        tools,
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(128),
        parallel_tool_calls: false,
    }
}

fn image_request(model: &str) -> GenRequest {
    serde_json::from_value(json!({
        "model": model,
        "messages": [{"role":"user","content":[
            {"type":"image","image":{"bytes":base64::engine::general_purpose::STANDARD.decode(IMAGE_PNG).unwrap(),"media_type":"png","width":1,"height":1}},
            {"type":"text","text":"describe"}
        ]}],
        "tools":[],"thinking":"off","max_output_tokens":128,"parallel_tool_calls":false
    }))
    .expect("valid multimodal request")
}

fn kimi_tools() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: "read_file".into(),
            description: "Read one file.".into(),
            parameters: json!({
                "type":"object",
                "properties":{"path":{"type":"string"}},
                "required":["path"]
            }),
        },
        ToolDescriptor {
            name: "$web_search".into(),
            description: String::new(),
            parameters: json!({"type":"object"}),
        },
    ]
}

fn anthropic_tools() -> Vec<ToolDescriptor> {
    kimi_tools().into_iter().take(1).collect()
}

fn response(body: &'static str) -> Result<HttpResponse, HttpError> {
    let chunks = body
        .as_bytes()
        .chunks(7)
        .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
        .collect::<Vec<_>>();
    Ok(HttpResponse {
        status: 200,
        headers: vec![HttpHeader::new("content-type", b"text/event-stream".to_vec()).unwrap()],
        body: Box::pin(futures::stream::iter(chunks)),
    })
}

#[test]
fn resolved_model_owns_the_wire_selected_by_authenticated_kimi_discovery() {
    let unresolved = ProviderRow::kimi_coding_default();
    assert!(
        !format!("{unresolved:?}").contains("wire"),
        "ProviderRow exposes a pre-resolution wire"
    );
    let (result, stub, _) = resolve("kimi-coding/kimi-default", KIMI_MODELS);
    let resolved = result.expect("advertised model resolves");

    // Public final API: ProviderRow has no wire to read, while the atomic
    // resolved value does.
    assert_eq!(format!("{:?}", resolved.wire), "KimiOpenAi");
    assert_eq!(resolved.model, "kimi-default");
    assert_eq!(resolved.capabilities.max_context_tokens, Some(131072));
    assert_eq!(stub.paths(), ["/coding/v1/models"]);
    let discovery = stub.asked().pop().expect("one discovery request");
    assert_eq!(discovery.method, ego_ai::http::Method::Get);
    assert_eq!(
        discovery
            .headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str()),
        Some("Bearer test-key")
    );
}

#[test]
fn absent_and_kimi_protocols_select_the_distinct_kimi_openai_mapping() {
    for model in ["kimi-default", "kimi-explicit"] {
        let (result, _, _) = resolve(&format!("kimi-coding/{model}"), KIMI_MODELS);
        let resolved = result.expect("current model resolves");
        assert_eq!(format!("{:?}", resolved.wire), "KimiOpenAi");
    }
}

#[test]
fn anthropic_protocol_selects_distinct_kimi_beta_messages_mapping() {
    let (result, _, resolved_credential) = resolve("kimi-coding/kimi-anthropic", KIMI_MODELS);
    let resolved = result.expect("current Anthropic model resolves");
    assert_eq!(format!("{:?}", resolved.wire), "KimiAnthropic");

    let headers = resolved_credential.headers();
    let target = RequestTarget::new(resolved.row.base(), &headers);
    let built = resolved
        .wire
        .build_request(&target, &request("kimi-anthropic", anthropic_tools()))
        .expect("Kimi Anthropic request is expressible");
    assert_eq!(
        built.url.as_str(),
        "https://api.kimi.com/coding/v1/messages?beta=true"
    );
    assert!(
        built
            .headers
            .iter()
            .any(|header| header.name() == "authorization" && header.value() == b"Bearer test-key")
    );
    assert!(
        built
            .headers
            .iter()
            .any(|header| header.name() == "anthropic-version" && !header.value().is_empty())
    );
    let body: Value = serde_json::from_slice(built.body.as_deref().unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::from_str::<Value>(KIMI_ANTHROPIC_REQUEST).unwrap()
    );
    assert_eq!(body["thinking"]["type"], "disabled");
}

#[test]
fn kimi_openai_fixture_pins_thinking_completion_tools_builtin_and_usage() {
    let (result, _, resolved_credential) = resolve("kimi-coding/kimi-default", KIMI_MODELS);
    let resolved = result.expect("current model resolves");
    let headers = resolved_credential.headers();
    let target = RequestTarget::new(resolved.row.base(), &headers);
    let built = resolved
        .wire
        .build_request(&target, &request("kimi-default", kimi_tools()))
        .expect("Kimi OpenAI request is expressible");
    assert_eq!(
        built.url.as_str(),
        "https://api.kimi.com/coding/v1/chat/completions"
    );
    assert!(
        built
            .headers
            .iter()
            .any(|header| header.name() == "authorization" && header.value() == b"Bearer test-key")
    );
    let body: Value = serde_json::from_slice(built.body.as_deref().unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::from_str::<Value>(KIMI_OPENAI_REQUEST).unwrap()
    );
    assert_eq!(body["thinking"]["type"], "disabled");
    assert_eq!(body["max_completion_tokens"], 128);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["tools"][1]["type"], "builtin_function");

    let events: Vec<AiEvent> = block_on(
        resolved
            .wire
            .decode(response(KIMI_OPENAI_STREAM), None)
            .collect(),
    );
    assert!(
        events
            .iter()
            .any(|event| { matches!(event, AiEvent::TextDelta { text } if text == "ok") })
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AiEvent::Usage(_)))
    );
}

#[test]
fn kimi_anthropic_fixture_decodes_usage_on_the_beta_messages_stream() {
    let (result, _, _) = resolve("kimi-coding/kimi-anthropic", KIMI_MODELS);
    let resolved = result.expect("current model resolves");
    let events: Vec<AiEvent> = block_on(
        resolved
            .wire
            .decode(response(KIMI_ANTHROPIC_STREAM), None)
            .collect(),
    );
    assert!(
        events
            .iter()
            .any(|event| { matches!(event, AiEvent::TextDelta { text } if text == "ok") })
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AiEvent::Usage(_)))
    );
}

/// The public `openai` row resolves, and this is the test that says so rather
/// than letting its absence from the table above look like a decision.
///
/// It used to assert the opposite. `2026-09-03-public-provider-wires-require-
/// live-promotion.md` withheld the row until conformance check five was
/// recorded against a live entitled model;
/// `2026-09-07-a-published-contract-is-enough-to-offer-a-row.md` amends that,
/// because the remedy the refusal named needed a source checkout the operator
/// it addressed may not have. Nothing about the evidence changed — no
/// recording exists — only where the missing check is paid for.
///
/// The claim is that no gate stands in front of resolution, so it is made
/// against `unasked_http()`: resolving this row must reach a wire without
/// asking anything of the network.
#[test]
fn the_public_openai_row_resolves_onto_its_published_responses_mapping() {
    let slug = "openai/gpt-5.6";
    let catalogue = Catalogue::new([ProviderRow::openai()]).with_hosted(
        slug,
        HostedModel {
            capabilities: supplied_capabilities(),
            authority: SuppliedAuthority::System,
            metering: tabulated(),
            rates: None,
        },
    );
    let resolved = block_on(catalogue.resolve(
        &ModelSlug::parse(slug).unwrap(),
        &unasked_http(),
        &credential(),
    ))
    .expect("a published contract is enough to offer the row");
    assert_eq!(resolved.model, "gpt-5.6");
}

#[test]
fn every_current_fixed_profile_also_installs_its_wire_on_resolved_model() {
    for (row, provider, model, wire) in [
        (
            ProviderRow::openai_codex_default(),
            "openai-codex",
            "gpt-5.6",
            "OpenAiCodexResponses",
        ),
        (
            ProviderRow::anthropic_default(),
            "anthropic",
            "claude-opus-5",
            "AnthropicMessages",
        ),
        (
            ProviderRow::github_copilot_default(),
            "github-copilot",
            "gpt-4.1",
            "GithubCopilotCompletions",
        ),
        (
            ProviderRow::minimax_oauth_default(),
            "minimax-oauth",
            "MiniMax-M2.7",
            "MiniMaxAnthropic",
        ),
        (
            ProviderRow::nous_default(),
            "nous",
            "hermes-3-70b",
            "NousPortal",
        ),
    ] {
        let slug = format!("{provider}/{model}");
        let catalogue = Catalogue::new([row]).with_hosted(
            &slug,
            HostedModel {
                capabilities: supplied_capabilities(),
                authority: SuppliedAuthority::System,
                metering: tabulated(),
                rates: None,
            },
        );
        let resolved = block_on(catalogue.resolve(
            &ModelSlug::parse(&slug).unwrap(),
            &unasked_http(),
            &credential(),
        ))
        .expect("fixed profile resolves from supplied facts");
        assert_eq!(
            format!("{:?}", resolved.wire),
            wire,
            "{provider} changed mapping"
        );
    }
}

#[test]
fn ollama_and_xai_current_resolvers_also_install_their_owned_wires() {
    let ollama = ProviderRow::ollama_default();
    let ollama_catalogue = Catalogue::new([ollama]);
    let (ollama_http, _) = server([
        (
            200,
            r#"{"models":[{"name":"qwen3","capabilities":["completion"]}]}"#,
        ),
        (
            200,
            r#"{"models":[{"name":"qwen3","context_length":32768}]}"#,
        ),
    ]);
    let ollama_resolved = block_on(ollama_catalogue.resolve(
        &ModelSlug::parse("ollama/qwen3").unwrap(),
        &ollama_http,
        &credential(),
    ))
    .expect("Ollama resolves its served model");
    assert_eq!(format!("{:?}", ollama_resolved.wire), "Ollama");

    let xai = ProviderRow::xai_oauth_default();
    let xai_catalogue = Catalogue::new([xai]).with_hosted(
        "xai-oauth/grok-4.6",
        HostedModel {
            capabilities: supplied_capabilities(),
            authority: SuppliedAuthority::System,
            metering: tabulated(),
            rates: None,
        },
    );
    let (xai_http, _) = server([(
        200,
        r#"{"models":[{"id":"grok-4.6","prompt_text_token_price":30000,"cached_prompt_text_token_price":7500,"completion_text_token_price":150000,"long_context_threshold":0}]}"#,
    )]);
    let xai_resolved = block_on(xai_catalogue.resolve(
        &ModelSlug::parse("xai-oauth/grok-4.6").unwrap(),
        &xai_http,
        &credential(),
    ))
    .expect("xAI resolves its published model price");
    assert_eq!(format!("{:?}", xai_resolved.wire), "XaiResponses");
}

#[test]
fn malformed_unknown_non_string_protocol_and_absent_model_refuse_before_generation() {
    for (model, document, expected) in [
        ("malformed-envelope", r#"{}"#, "data"),
        (
            "unknown",
            r#"{"data":[{"id":"unknown","context_length":131072,"supports_reasoning":false,"supports_image_in":false,"supports_video_in":false,"supports_tool_use":false,"protocol":"future-wire"}]}"#,
            "future-wire",
        ),
        (
            "non-string",
            r#"{"data":[{"id":"non-string","context_length":131072,"supports_reasoning":false,"supports_image_in":false,"supports_video_in":false,"supports_tool_use":false,"protocol":true}]}"#,
            "protocol",
        ),
        (
            "missing-tool-fact",
            r#"{"data":[{"id":"missing-tool-fact","context_length":131072,"supports_reasoning":false,"supports_image_in":false,"supports_video_in":false}]}"#,
            "supports_tool_use",
        ),
        (
            "broken",
            r#"{"data":[{"id":"broken","context_length":0,"supports_reasoning":false,"supports_image_in":false,"supports_video_in":false,"supports_tool_use":false}]}"#,
            "context",
        ),
    ] {
        let (result, stub, _) = resolve(&format!("kimi-coding/{model}"), document);
        let error = result
            .expect_err("invalid current model data was accepted")
            .to_string();
        assert!(error.contains(expected), "refusal lost {expected}: {error}");
        assert_eq!(stub.paths(), ["/coding/v1/models"]);
    }

    let (result, stub, _) = resolve("kimi-coding/not-advertised", KIMI_MODELS);
    let error = result
        .expect_err("absent model was resurrected")
        .to_string();
    assert!(
        error.contains("not-advertised"),
        "requested model omitted: {error}"
    );
    assert!(
        error.contains("kimi-default"),
        "advertised set omitted: {error}"
    );
    assert_eq!(stub.paths(), ["/coding/v1/models"]);
}

#[test]
fn kimi_openai_normalizes_every_current_nested_schema_slot_without_resolving_refs() {
    let schema = json!({
        "type":"object",
        "properties":{"root":{"properties":{"value":{}}}},
        "dependencies":{"legacy":{"properties":{"value":{}}}},
        "dependentSchemas":{"dependent":{"properties":{"value":{}}}},
        "additionalItems":{"properties":{"value":{}}},
        "contentSchema":{"properties":{"value":{}}},
        "propertyNames":{"properties":{"value":{}}},
        "unevaluatedItems":{"properties":{"value":{}}},
        "unevaluatedProperties":{"properties":{"value":{}}},
        "items":[{"properties":{"tuple":{}}}],
        "definitions":{"Cyclic":{"$ref":"#/definitions/Cyclic"}},
        "unresolved":{"$ref":"#/definitions/Missing"}
    });
    let (result, _, credential) = resolve("kimi-coding/kimi-default", KIMI_MODELS);
    let resolved = result.expect("current Kimi model resolves");
    let headers = credential.headers();
    let target = RequestTarget::new(resolved.row.base(), &headers);
    let built = resolved
        .wire
        .build_request(
            &target,
            &request(
                "kimi-default",
                vec![ToolDescriptor {
                    name: "schema_tool".into(),
                    description: "schema test".into(),
                    parameters: schema,
                }],
            ),
        )
        .expect("current JSON Schema can be represented");
    let body: Value = serde_json::from_slice(built.body.as_deref().unwrap()).unwrap();
    let normalized = &body["tools"][0]["function"]["parameters"];
    for path in [
        ["properties", "root"],
        ["dependencies", "legacy"],
        ["dependentSchemas", "dependent"],
    ] {
        assert_eq!(
            normalized.pointer(&format!("/{}", path.join("/"))).unwrap()["type"],
            "object"
        );
    }
    for key in [
        "additionalItems",
        "contentSchema",
        "propertyNames",
        "unevaluatedItems",
        "unevaluatedProperties",
    ] {
        assert_eq!(
            normalized[key]["type"], "object",
            "{key} was not normalized"
        );
    }
    assert_eq!(normalized["items"][0]["type"], "object");
    assert_eq!(
        normalized["definitions"]["Cyclic"]["$ref"],
        "#/definitions/Cyclic"
    );
    assert_eq!(normalized["unresolved"]["$ref"], "#/definitions/Missing");
}

#[test]
fn resolved_kimi_wires_pin_full_image_request_bytes_and_route_distinctions() {
    let (result, _, credential) = resolve("kimi-coding/kimi-default", KIMI_MODELS);
    let openai = result.expect("current Kimi OpenAI model resolves");
    let headers = credential.headers();
    let target = RequestTarget::new(openai.row.base(), &headers);
    let built = openai
        .wire
        .build_request(&target, &image_request("kimi-default"))
        .expect("Kimi OpenAI image request");
    assert_eq!(
        built.url.as_str(),
        "https://api.kimi.com/coding/v1/chat/completions"
    );
    assert!(
        built
            .headers
            .iter()
            .any(|header| header.name() == "authorization" && header.value() == b"Bearer test-key")
    );
    let body: Value = serde_json::from_slice(built.body.as_deref().unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::from_str::<Value>(KIMI_OPENAI_IMAGE_REQUEST).unwrap()
    );

    let (result, _, credential) = resolve("kimi-coding/kimi-anthropic", KIMI_MODELS);
    let anthropic = result.expect("current Kimi Anthropic model resolves");
    let headers = credential.headers();
    let target = RequestTarget::new(anthropic.row.base(), &headers);
    let built = anthropic
        .wire
        .build_request(&target, &image_request("kimi-anthropic"))
        .expect("Kimi Anthropic image request");
    assert_eq!(
        built.url.as_str(),
        "https://api.kimi.com/coding/v1/messages?beta=true"
    );
    assert!(
        built
            .headers
            .iter()
            .any(|header| header.name() == "anthropic-version" && !header.value().is_empty())
    );
    assert!(
        built
            .headers
            .iter()
            .any(|header| header.name() == "authorization" && header.value() == b"Bearer test-key")
    );
    let body: Value = serde_json::from_slice(built.body.as_deref().unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::from_str::<Value>(KIMI_ANTHROPIC_IMAGE_REQUEST).unwrap()
    );
}

#[test]
fn rejected_auth_is_not_missing_and_never_falls_back_to_generation() {
    for status in [401, 402, 403] {
        let catalogue = Catalogue::new([ProviderRow::kimi_coding_default()]);
        let (http, stub) = server([(status, r#"{"error":"expired"}"#)]);
        let result = block_on(catalogue.resolve(
            &ModelSlug::parse("kimi-coding/kimi-default").unwrap(),
            &http,
            &credential(),
        ));
        let error = result.expect_err("rejected auth was accepted").to_string();
        assert!(error.contains("kimi-coding") && error.contains(&status.to_string()));
        assert_eq!(stub.paths(), ["/coding/v1/models"]);
    }
}

#[test]
fn rejected_kimi_credentials_are_typed_and_render_relogin_without_secret_material() {
    for status in [401, 402, 403] {
        let catalogue = Catalogue::new([ProviderRow::kimi_coding_default()]);
        let (http, _) = server([(status, r#"{"error":"credential-body-secret"}"#)]);
        let error = block_on(catalogue.resolve(
            &ModelSlug::parse("kimi-coding/kimi-default").unwrap(),
            &http,
            &credential(),
        ))
        .expect_err("rejected credential was accepted");
        match &error {
            ego_ai::catalogue::ResolveError::RejectedCredential {
                provider,
                status: actual,
            } => {
                assert_eq!(provider, "kimi-coding");
                assert_eq!(*actual, status);
            }
            other => panic!("status {status} was not typed credential rejection: {other:?}"),
        }
        let rendered = error.to_string();
        assert!(rendered.contains("ego auth login kimi-coding"));
        assert!(!rendered.contains("credential-body-secret"));
        assert!(!rendered.contains("test-key"));
    }
}

/// regression case: the statuses *outside* the 401/402/403 carve-out are scrubbed too.
///
/// `rejected_kimi_credentials_are_typed_and_render_relogin_without_secret_material`
/// above proves the credential-rejection branch, which was the only body-free
/// one. Every other non-2xx fell through to `EndpointUnreachable` carrying
/// `String::from_utf8_lossy` of whatever the endpoint answered — and a
/// catalogue request carries the operator's key, so an endpoint that echoes it,
/// or a proxy that reflects the request headers into an interception page, put
/// that material on a terminal and into the ACP host's `error.data`.
#[test]
fn catalogue_refusals_outside_the_credential_carve_out_are_scrubbed_but_keep_the_status() {
    const ECHOED: &str = r#"{"error":"upstream said: authorization Bearer test-key","upstream_body_marker":"catalogue-body-secret"}"#;
    for status in [400, 404, 429, 500, 503] {
        let catalogue = Catalogue::new([ProviderRow::kimi_coding_default()]);
        let (http, _) = server([(status, ECHOED)]);
        let error = block_on(catalogue.resolve(
            &ModelSlug::parse("kimi-coding/kimi-default").unwrap(),
            &http,
            &credential(),
        ))
        .expect_err("a refused catalogue answer was accepted");
        match &error {
            ego_ai::catalogue::ResolveError::EndpointUnreachable { endpoint, message } => {
                assert!(
                    endpoint.contains("kimi-coding"),
                    "{status}: the endpoint does not name the provider: {endpoint}"
                );
                assert!(
                    message.contains(&status.to_string()),
                    "{status}: the message drops the status: {message}"
                );
            }
            other => panic!("status {status} was not an endpoint refusal: {other:?}"),
        }
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(
                !rendered.contains("catalogue-body-secret"),
                "{status} leaked the body: {rendered}"
            );
            assert!(
                !rendered.contains("test-key"),
                "{status} leaked the credential: {rendered}"
            );
            assert!(
                !rendered.contains("upstream_body_marker"),
                "{status} leaked the body: {rendered}"
            );
        }
    }
}

#[test]
fn kimi_streams_have_one_terminal_stop_exact_usage_and_complete_tool_calls() {
    let (result, _, _) = resolve("kimi-coding/kimi-default", KIMI_MODELS);
    let openai = result.expect("current model resolves");
    let openai_events: Vec<AiEvent> = block_on(
        openai
            .wire
            .decode(response(KIMI_OPENAI_TOOL_STREAM), None)
            .collect(),
    );
    assert_eq!(
        openai_events
            .iter()
            .filter(|event| matches!(event, AiEvent::Stop(_)))
            .count(),
        1
    );
    assert!(matches!(
        openai_events.last(),
        Some(AiEvent::Stop(
            ego_core::generator::types::StopReason::ToolUse
        ))
    ));
    assert!(openai_events.iter().any(|event| matches!(event, AiEvent::ToolCall { call_id, name, args } if call_id == "call_kimi" && name == "read_file" && args == r#"{"path":"README.md"}"#)));
    assert!(openai_events.iter().any(|event| matches!(event, AiEvent::Usage(usage) if usage.uncached_input_tokens == 10 && usage.output_tokens == 4 && usage.total_tokens() == 14)));

    let (result, _, _) = resolve("kimi-coding/kimi-anthropic", KIMI_MODELS);
    let anthropic = result.expect("current model resolves");
    let anthropic_events: Vec<AiEvent> = block_on(
        anthropic
            .wire
            .decode(response(KIMI_ANTHROPIC_TOOL_STREAM), None)
            .collect(),
    );
    assert_eq!(
        anthropic_events
            .iter()
            .filter(|event| matches!(event, AiEvent::Stop(_)))
            .count(),
        1
    );
    assert!(matches!(
        anthropic_events.last(),
        Some(AiEvent::Stop(
            ego_core::generator::types::StopReason::ToolUse
        ))
    ));
    assert!(anthropic_events.iter().any(|event| matches!(event, AiEvent::ToolCall { call_id, name, args } if call_id == "toolu_kimi" && name == "read_file" && args == r#"{"path":"README.md"}"#)));
    assert!(anthropic_events.iter().any(|event| matches!(event, AiEvent::Usage(usage) if usage.uncached_input_tokens == 700 && usage.cache_read_tokens == 200 && usage.cache_write_tokens == 100 && usage.output_tokens == 4 && usage.total_tokens() == 1004)));
}

#[test]
fn kimi_stream_refusal_errors_are_terminal_and_never_silent() {
    for (model, fixture) in [
        ("kimi-coding/kimi-default", KIMI_OPENAI_ERROR_STREAM),
        ("kimi-coding/kimi-anthropic", KIMI_ANTHROPIC_ERROR_STREAM),
    ] {
        let (result, _, _) = resolve(model, KIMI_MODELS);
        let resolved = result.expect("current model resolves");
        let events: Vec<AiEvent> =
            block_on(resolved.wire.decode(response(fixture), None).collect());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::Error { .. }))
        );
        assert!(matches!(
            events.last(),
            Some(AiEvent::Stop(ego_core::generator::types::StopReason::Error))
        ));
    }
}

#[test]
fn current_models_override_cache_and_models_toml_for_generation_authority() {
    let current = r#"{"data":[{"id":"fresh","context_length":131072,"supports_reasoning":false,"supports_image_in":false,"supports_video_in":false,"supports_tool_use":false}]}"#;
    let catalogue = Catalogue::new([ProviderRow::kimi_coding_default()]).with_hosted(
        "kimi-coding/stale-from-cache",
        HostedModel {
            capabilities: supplied_capabilities(),
            authority: SuppliedAuthority::System,
            metering: tabulated(),
            rates: None,
        },
    );
    let body: &'static str = Box::leak(current.to_string().into_boxed_str());
    let (http, stub) = server([(200, body)]);
    let result = block_on(catalogue.resolve(
        &ModelSlug::parse("kimi-coding/stale-from-cache").unwrap(),
        &http,
        &credential(),
    ));
    let error = result
        .expect_err("cache resurrected a model absent from current /models")
        .to_string();
    assert!(error.contains("stale-from-cache"));
    assert_eq!(stub.paths(), ["/coding/v1/models"]);
}

#[test]
fn missing_kimi_credential_refuses_before_discovery() {
    let row = ProviderRow::kimi_coding_default();
    let error = row
        .credential
        .resolve(&FakeCredentials::empty())
        .expect_err("missing Kimi credential was accepted");
    assert!(error.to_string().contains("ego auth login kimi-coding"));
}

#[test]
fn qwen_oauth_has_no_selectable_compatibility_surface() {
    // ego-ai has no public provider inventory API. This negative inventory
    // check is therefore limited to the selectable source surfaces; all
    // positive regression case assertions use the public resolution API.
    // `qwen` alone is not retired: Ollama's current local `ollama/qwen3`
    // model is a valid selectable surface. Match the removed OAuth contract,
    // not that unrelated model family.
    let retired_markers = ["qwen-oauth", "qwen_portal", "portal.qwen.ai"];
    let provider = include_str!("../src/provider.rs");
    let model_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ego-cli/src/model.rs");
    let model = std::fs::read_to_string(&model_path).expect("CLI model source exists");
    let oauth = include_str!("../src/oauth.rs");
    let wire_mod = include_str!("../src/wire/mod.rs");
    for (name, source) in [
        ("provider", provider),
        ("CLI model", model.as_str()),
        ("OAuth", oauth),
        ("wire module", wire_mod),
    ] {
        let source = source.to_ascii_lowercase();
        assert!(
            !retired_markers.iter().any(|marker| source.contains(marker)),
            "{name} still exposes retired Qwen OAuth"
        );
    }
    let wire = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/wire/qwen_portal.rs");
    assert!(!wire.exists(), "retired Qwen mapping remains on disk");
}

fn unasked_http() -> Arc<dyn Http> {
    server(Vec::<Answer>::new()).0
}

// Catches: OpenRouter generation still requires a builtin row or imports a price.
#[test]
fn openrouter_current_inventory_resolves_its_owned_wire_without_prices() {
    let catalogue = Catalogue::new([ProviderRow::openrouter_default()]);
    let (http, stub) = server([(
        200,
        include_str!("../../ego-cli/tests/fixtures/openrouter-models.json"),
    )]);
    let resolved = block_on(catalogue.resolve(
        &ModelSlug::parse("openrouter/meta/muse-wonder").unwrap(),
        &http,
        &credential(),
    ))
    .unwrap();
    assert_eq!(format!("{:?}", resolved.wire), "OpenRouterCompletions");
    assert_eq!(resolved.capabilities.max_context_tokens, Some(1048576));
    assert_eq!(resolved.capabilities.max_output_tokens, Some(943718));
    assert!(resolved.rates.is_none());
    assert_eq!(stub.paths(), ["/api/v1/models"]);
}

// Catches: discovery drops the operator admission quote or replaces reported settlement.
#[test]
fn openrouter_resolution_preserves_operator_quote() {
    use ego_ai::catalogue::operator_rates;
    use ego_core::budget::{Metering, PricingQuote, SettlementSource};
    use ego_core::cost::{CurrencyCode, PricingRevision};

    let rates = operator_rates(PricingQuote {
        revision: PricingRevision("operator-openrouter-pin".to_owned()),
        currency: CurrencyCode::new("USD").unwrap(),
        uncached_input: Some("0.000002".parse().unwrap()),
        cache_read: Some("0.0000001".parse().unwrap()),
        cache_write: Some("0.0000025".parse().unwrap()),
        cache_write_extended: Some("0.000004".parse().unwrap()),
        output: Some("0.00001".parse().unwrap()),
        web_search: None,
    });
    let catalogue = Catalogue::new([ProviderRow::openrouter_default()]).with_hosted(
        "openrouter/anthropic/claude-sonnet-5.5",
        HostedModel {
            capabilities: supplied_capabilities(),
            authority: SuppliedAuthority::User,
            metering: tabulated(),
            rates: Some(rates.clone()),
        },
    );
    let (http, stub) = server([(
        200,
        include_str!("goldens/openrouter/models-2026-10-15.json"),
    )]);
    let resolved = block_on(catalogue.resolve(
        &ModelSlug::parse("openrouter/anthropic/claude-sonnet-5.5").unwrap(),
        &http,
        &credential(),
    ))
    .unwrap();
    assert_eq!(resolved.rates, Some(rates));
    assert!(matches!(
        resolved.metering,
        Metering::Currency {
            settlement: SettlementSource::ProviderReported { .. }
        }
    ));
    assert_eq!(stub.paths(), ["/api/v1/models"]);
}

// Catches: an absent quote is fabricated or copied from a different routed slug.
#[test]
fn openrouter_resolution_does_not_borrow_another_models_quote() {
    use ego_ai::catalogue::operator_rates;
    use ego_core::budget::PricingQuote;
    use ego_core::cost::{CurrencyCode, PricingRevision};

    let catalogue = Catalogue::new([ProviderRow::openrouter_default()])
        .with_hosted(
            "openrouter/anthropic/claude-sonnet-5.5",
            HostedModel {
                capabilities: supplied_capabilities(),
                authority: SuppliedAuthority::User,
                metering: tabulated(),
                rates: Some(operator_rates(PricingQuote {
                    revision: PricingRevision("other-model-only".to_owned()),
                    currency: CurrencyCode::new("USD").unwrap(),
                    uncached_input: Some("0.000002".parse().unwrap()),
                    cache_read: None,
                    cache_write: None,
                    cache_write_extended: None,
                    output: Some("0.00001".parse().unwrap()),
                    web_search: None,
                })),
            },
        )
        .with_hosted(
            "openrouter/openai/gpt-6-luna",
            HostedModel {
                capabilities: supplied_capabilities(),
                authority: SuppliedAuthority::User,
                metering: tabulated(),
                rates: None,
            },
        );
    for slug in [
        "openrouter/openai/gpt-6-luna",
        "openrouter/meta/muse-wonder",
    ] {
        let (http, _) = server([(
            200,
            include_str!("goldens/openrouter/models-2026-10-15.json"),
        )]);
        let resolved =
            block_on(catalogue.resolve(&ModelSlug::parse(slug).unwrap(), &http, &credential()))
                .unwrap();
        assert!(resolved.rates.is_none(), "{slug} has no operator quote");
    }
}

// Catches: published OpenRouter reasoning support is discarded and low is refused locally.
#[test]
fn openrouter_published_reasoning_admits_low() {
    for model in ["openai/gpt-6-luna", "anthropic/claude-sonnet-5.5"] {
        let catalogue = Catalogue::new([ProviderRow::openrouter_default()]);
        let (http, _) = server([(
            200,
            include_str!("goldens/openrouter/models-2026-10-15.json"),
        )]);
        let resolved = block_on(catalogue.resolve(
            &ModelSlug::parse(&format!("openrouter/{model}")).unwrap(),
            &http,
            &credential(),
        ))
        .unwrap();
        assert_eq!(
            resolved
                .capabilities
                .resolve_reasoning_effort(Some(ThinkingLevel::Low)),
            Ok(ThinkingLevel::Low),
            "{model} publishes reasoning in supported_parameters"
        );
        let mut req = request(model, vec![]);
        req.thinking = ThinkingLevel::Low;
        assert!(resolved.capabilities.request_gaps(&req).is_empty());
        let body: Value = serde_json::from_slice(
            &ego_ai::wire::openai_completions::openrouter_request_body(&req).unwrap(),
        )
        .unwrap();
        assert_eq!(body["reasoning_effort"], "low");
        // Claude's published list excludes minimal and none; do not generalize the vocabulary.
        assert!(
            resolved
                .capabilities
                .resolve_reasoning_effort(Some(ThinkingLevel::Minimal))
                .is_err()
        );
    }
}

// Catches: a non-reasoning inventory row inherits a paid reasoning default or accepts low.
#[test]
fn openrouter_absent_reasoning_defaults_off_and_rejects_explicit_low() {
    let catalogue = Catalogue::new([ProviderRow::openrouter_default()]);
    let (http, _) = server([(
        200,
        include_str!("goldens/openrouter/models-2026-10-15.json"),
    )]);
    let resolved = block_on(catalogue.resolve(
        &ModelSlug::parse("openrouter/~unbiased/pareto-latest").unwrap(),
        &http,
        &credential(),
    ))
    .unwrap();
    assert_eq!(
        resolved.capabilities.resolve_reasoning_effort(None),
        Ok(ThinkingLevel::Off)
    );
    assert!(
        resolved
            .capabilities
            .resolve_reasoning_effort(Some(ThinkingLevel::Low))
            .is_err()
    );
    let req = request(&resolved.model, vec![]);
    assert!(resolved.capabilities.request_gaps(&req).is_empty());
    let body: Value = serde_json::from_slice(
        &ego_ai::wire::openai_completions::openrouter_request_body(&req).unwrap(),
    )
    .unwrap();
    assert!(body.get("reasoning_effort").is_none());
    assert_eq!(body["reasoning"], json!({"enabled": false}));
}

// Catches: a reasoning parameter alone fabricates effort support; a missing field grants low.
#[test]
fn openrouter_incomplete_reasoning_facts_do_not_invent_efforts() {
    for document in [
        r#"{"data":[{"id":"fixture/model"}]}"#,
        r#"{"data":[{"id":"fixture/model","supported_parameters":["reasoning"]}]}"#,
        r#"{"data":[{"id":"fixture/model","supported_parameters":["reasoning"],"reasoning":{"supported_efforts":["future-effort"]}}]}"#,
    ] {
        let catalogue = Catalogue::new([ProviderRow::openrouter_default()]);
        let (http, _) = server([(200, document)]);
        let resolved = block_on(catalogue.resolve(
            &ModelSlug::parse("openrouter/fixture/model").unwrap(),
            &http,
            &credential(),
        ))
        .unwrap();
        assert!(
            resolved
                .capabilities
                .resolve_reasoning_effort(Some(ThinkingLevel::Low))
                .is_err()
        );
    }
}

// Catches: discovery replaces an operator's stronger capability facts.
#[test]
fn openrouter_supplied_reasoning_overrides_discovery() {
    use ego_core::generator::capabilities::ReasoningEffortSupport;
    let mut capabilities = supplied_capabilities();
    capabilities.reasoning_efforts = ReasoningEffortSupport::Only([ThinkingLevel::Off].into());
    capabilities.default_reasoning_effort = Some(ThinkingLevel::Off);
    let catalogue = Catalogue::new([ProviderRow::openrouter_default()]).with_hosted(
        "openrouter/openai/gpt-6-luna",
        HostedModel {
            capabilities,
            authority: SuppliedAuthority::User,
            metering: ego_ai::catalogue::subscription("fixture"),
            rates: None,
        },
    );
    let (http, _) = server([(
        200,
        include_str!("goldens/openrouter/models-2026-10-15.json"),
    )]);
    let resolved = block_on(catalogue.resolve(
        &ModelSlug::parse("openrouter/openai/gpt-6-luna").unwrap(),
        &http,
        &credential(),
    ))
    .unwrap();
    assert_eq!(
        resolved.capabilities.resolve_reasoning_effort(None),
        Ok(ThinkingLevel::Off)
    );
    assert!(
        resolved
            .capabilities
            .resolve_reasoning_effort(Some(ThinkingLevel::Low))
            .is_err()
    );
}

// Catches: omitting Off's effort leaves OpenRouter's default-enabled reasoning active.
#[test]
fn openrouter_off_disables_reasoning_even_when_the_model_defaults_on() {
    let req = request("openai/gpt-6-luna", vec![]);
    let body: Value = serde_json::from_slice(
        &ego_ai::wire::openai_completions::openrouter_request_body(&req).unwrap(),
    )
    .unwrap();
    assert_eq!(body["reasoning"], json!({"enabled": false}));
    assert!(body.get("reasoning_effort").is_none());
}
