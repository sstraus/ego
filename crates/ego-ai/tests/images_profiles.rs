//! RED contract for image capability refusal and every current profile disposition.

use base64::Engine as _;
use ego_ai::catalogue::{Catalogue, HostedModel, ModelSlug, SuppliedAuthority, tabulated};
use ego_ai::provider::ProviderRow;
use ego_ai::wire::ollama::Ollama;
use ego_ai::wire::openai_codex_responses::OpenAiCodexResponses;
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::generator::types::{GenRequest, ThinkingLevel};
use ego_core::permit::HttpHeader;
use serde_json::{Value, json};

mod support;
use support::{StubServer, credential, server, unasked};

const KIMI_MODELS: &str = include_str!("goldens/kimi/models/current.json");

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn request() -> GenRequest {
    serde_json::from_value(json!({
        "model":"fixture-vision",
        "messages":[{"role":"user","content":[
            {"type":"image","image":{"bytes":base64::engine::general_purpose::STANDARD.decode(PNG).unwrap(),"media_type":"png","width":1,"height":1}},
            {"type":"text","text":"describe"}
        ]}],
        "tools":[],"thinking":"off","max_output_tokens":128,"parallel_tool_calls":false
    })).expect("image request")
}

fn body(row: &ProviderRow, wire: &dyn WireFormat) -> Result<Value, String> {
    body_for(row, wire, &request())
}

fn body_for(
    row: &ProviderRow,
    wire: &dyn WireFormat,
    request: &GenRequest,
) -> Result<Value, String> {
    let headers = vec![HttpHeader::new("authorization", b"Bearer test".to_vec()).unwrap()];
    let target = RequestTarget::new(row.base(), &headers);
    let built = wire
        .build_request(&target, request)
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(built.body.as_deref().expect("JSON request body"))
        .map_err(|e| e.to_string())
}

fn xai_request() -> GenRequest {
    let mut request = request();
    request.thinking = ThinkingLevel::Low;
    request
}

fn kimi_request(model: &str) -> GenRequest {
    let mut request = request();
    request.model = model.to_string();
    request
}

fn resolve_kimi(
    model: &str,
) -> (
    ego_ai::catalogue::ResolvedModel,
    ego_ai::provider::ResolvedCredential,
    std::sync::Arc<StubServer>,
) {
    let catalogue = Catalogue::new([ProviderRow::kimi_coding_default()]);
    let credential = credential();
    let body: &'static str = Box::leak(KIMI_MODELS.to_string().into_boxed_str());
    let (http, stub) = server([(200, body)]);
    let resolved = futures::executor::block_on(catalogue.resolve(
        &ModelSlug::parse(&format!("kimi-coding/{model}")).unwrap(),
        &http,
        &credential,
    ))
    .expect("current Kimi model resolves");
    (resolved, credential, stub)
}

fn resolve_minimax() -> (
    ego_ai::catalogue::ResolvedModel,
    ego_ai::provider::ResolvedCredential,
) {
    let slug = ModelSlug::parse("minimax-oauth/MiniMax-M2.7").unwrap();
    let catalogue = Catalogue::new([ProviderRow::minimax_oauth_default()]).with_hosted(
        "minimax-oauth/MiniMax-M2.7",
        HostedModel {
            capabilities: serde_json::from_value(json!({
                "schema_version":4,"max_context_tokens":131072,"max_output_tokens":4096,
                "max_tools":16,"supports_parallel_tool_calls":false,
                "reasoning_carryover":"unsupported","reasoning_efforts":"any",
                "reasoning_state_dialect":null,"cancel":"cooperative","cache":"none",
                "transport":"sse","image_input":"unsupported"
            }))
            .expect("MiniMax text-only capability facts"),
            authority: SuppliedAuthority::System,
            metering: tabulated(),
            rates: None,
        },
    );
    let credential = credential();
    let (http, _) = unasked();
    let resolved = futures::executor::block_on(catalogue.resolve(&slug, &http, &credential))
        .expect("MiniMax resolves through its subscription resolver");
    (resolved, credential)
}

#[test]
fn openai_openrouter_and_ollama_use_inline_data_urls_and_keep_image_before_text() {
    for (row, wire) in [
        (ProviderRow::openai(), &OpenAiCompletions as &dyn WireFormat),
        (
            ProviderRow::openrouter_default(),
            &OpenAiCompletions as &dyn WireFormat,
        ),
        (ProviderRow::ollama_default(), &Ollama as &dyn WireFormat),
    ] {
        let body = body(&row, wire).expect("known chat profile expresses image input");
        let parts = body["messages"][0]["content"]
            .as_array()
            .expect("multimodal content parts");
        assert_eq!(
            parts[0]["type"],
            "image_url",
            "{} changed image part type",
            row.id()
        );
        assert!(
            parts[0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(parts[1]["type"], "text", "{} reordered text", row.id());
        assert!(!body.to_string().contains("fixture-vision.png"));
    }
}

#[test]
fn anthropic_uses_base64_image_source_before_text() {
    let row = ProviderRow::anthropic_default();
    let body = body(&row, &ego_ai::wire::anthropic_messages::AnthropicMessages)
        .expect("Anthropic vision contract");
    let content = body["messages"][0]["content"]
        .as_array()
        .expect("content parts");
    assert_eq!(content[0]["type"], "image");
    assert_eq!(content[0]["source"]["type"], "base64");
    assert_eq!(content[0]["source"]["media_type"], "image/png");
    assert_eq!(content[1]["type"], "text");
}

#[test]
fn xai_responses_uses_input_image_and_input_text_without_remote_origin() {
    let row = ProviderRow::xai_oauth_default();
    let body = body_for(
        &row,
        &ego_ai::wire::xai_responses::XaiResponses,
        &xai_request(),
    )
    .expect("xAI current Responses image contract");
    let content = body["input"][0]["content"]
        .as_array()
        .expect("input content parts");
    assert_eq!(content[0]["type"], "input_image");
    assert!(
        content[0]["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    assert_eq!(content[1]["type"], "input_text");
    assert!(!body.to_string().contains("http"));
}

#[test]
fn private_or_unproved_profiles_refuse_images_instead_of_dropping_them() {
    for row in [
        ProviderRow::openai_codex_default(),
        ProviderRow::github_copilot_default(),
        ProviderRow::nous_default(),
    ] {
        let wire: &dyn WireFormat = match row.id() {
            "openai-codex" => &OpenAiCodexResponses,
            "github-copilot" => &ego_ai::wire::github_copilot_completions::GithubCopilotCompletions,
            "nous" => &ego_ai::wire::nous_portal::NousPortal,
            other => panic!("unexpected refusal fixture {other}"),
        };
        let error = body(&row, wire).expect_err("unproved profile silently accepted image input");
        assert!(
            error.to_ascii_lowercase().contains("image"),
            "{} refusal is not image-specific: {error}",
            row.id()
        );
    }

    let (resolved, resolved_credential) = resolve_minimax();
    assert_eq!(format!("{:?}", resolved.wire), "MiniMaxAnthropic");
    let headers = resolved_credential.headers();
    let target = RequestTarget::new(resolved.row.base(), &headers);
    let error = resolved
        .wire
        .build_request(&target, &request())
        .expect_err("resolved MiniMax accepted unsupported image input")
        .to_string();
    assert!(
        error.to_ascii_lowercase().contains("unsupported"),
        "MiniMax refusal lost its capability reason: {error}"
    );
}

#[test]
fn image_wire_refusal_happens_before_any_provider_request_body_is_emitted() {
    let row = ProviderRow::openai_codex_default();
    let refused = OpenAiCodexResponses
        .build_request(&RequestTarget::new(row.base(), &[]), &request())
        .unwrap_err();
    assert!(refused.to_string().contains("image"));
}

#[test]
fn kimi_openai_and_anthropic_image_contracts_resolve_through_authenticated_models() {
    let (openai, openai_credential, openai_stub) = resolve_kimi("kimi-default");
    assert_eq!(format!("{:?}", openai.wire), "KimiOpenAi");
    assert_eq!(openai_stub.paths(), ["/coding/v1/models"]);
    let openai_headers = openai_credential.headers();
    let openai_target = RequestTarget::new(openai.row.base(), &openai_headers);
    let openai_body: Value = serde_json::from_slice(
        openai
            .wire
            .build_request(&openai_target, &kimi_request("kimi-default"))
            .expect("Kimi OpenAI supports image input")
            .body
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        openai_body["messages"][0]["content"][0]["type"],
        "image_url"
    );
    assert!(
        openai_body["messages"][0]["content"][0]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );

    let (anthropic, anthropic_credential, anthropic_stub) = resolve_kimi("kimi-anthropic");
    assert_eq!(format!("{:?}", anthropic.wire), "KimiAnthropic");
    assert_eq!(anthropic_stub.paths(), ["/coding/v1/models"]);
    let anthropic_headers = anthropic_credential.headers();
    let anthropic_target = RequestTarget::new(anthropic.row.base(), &anthropic_headers);
    let anthropic_body: Value = serde_json::from_slice(
        anthropic
            .wire
            .build_request(&anthropic_target, &kimi_request("kimi-anthropic"))
            .expect("Kimi Anthropic supports image input")
            .body
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(anthropic_body["messages"][0]["content"][0]["type"], "image");
    assert_eq!(
        anthropic_body["messages"][0]["content"][0]["source"]["type"],
        "base64"
    );
    assert_eq!(
        anthropic_body["messages"][0]["content"][0]["source"]["media_type"],
        "image/png"
    );
}

#[test]
fn authenticated_kimi_model_facts_refuse_image_for_models_that_do_not_advertise_it() {
    let (resolved, _, stub) = resolve_kimi("kimi-explicit");
    assert_eq!(format!("{:?}", resolved.wire), "KimiOpenAi");
    assert_eq!(stub.paths(), ["/coding/v1/models"]);
    let gaps = resolved
        .capabilities
        .request_gaps(&kimi_request("kimi-explicit"));
    assert!(
        gaps.iter()
            .any(|gap| gap.to_string().to_ascii_lowercase().contains("image")),
        "Kimi model without supports_image_in admitted an image: {gaps:?}"
    );
}
