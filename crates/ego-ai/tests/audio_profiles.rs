//! Pure audio profile and wire contracts. No transport or paid generation.
use base64::Engine as _;
use ego_ai::catalogue::{Catalogue, HostedModel, ModelSlug, SuppliedAuthority, tabulated};
use ego_ai::provider::ProviderRow;
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::generator::capabilities::{AudioInputSupport, ModelCapabilities};
use ego_core::generator::types::GenRequest;
use serde_json::{Value, json};
const WAV: &[u8] = include_bytes!("../../ego-core/tests/fixtures/audio/pcm16-mono.wav");
fn request() -> GenRequest {
    serde_json::from_value(json!({"model":"gpt-audio-1.5", "messages":[{"role":"user","content":[
        {"type":"text","text":"before"}, {"type":"audio","audio":{"bytes":WAV,"media_type":"wav"}}, {"type":"text","text":"after"}
    ]}], "tools":[],"thinking":"off","parallel_tool_calls":false})).unwrap()
}
fn body(row: &ProviderRow, wire: &dyn WireFormat, req: &GenRequest) -> Result<Value, String> {
    let target = RequestTarget::new(row.base(), &[]);
    let built = wire
        .build_request(&target, req)
        .map_err(|error| error.to_string())?;
    Ok(serde_json::from_slice(built.body.as_deref().unwrap()).unwrap())
}
// Catches serialization omitting, transcoding or reordering audio, or sending unsupported reasoning controls.
#[test]
fn public_openai_audio_uses_exact_wav_bytes_and_preserves_text_order() {
    let body = body(&ProviderRow::openai(), &OpenAiCompletions, &request()).unwrap();
    assert!(body.get("reasoning_effort").is_none());
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["content"], "before");
    assert_eq!(messages[1]["content"][0]["type"], "input_audio");
    assert_eq!(messages[1]["content"][0]["input_audio"]["format"], "wav");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(
                messages[1]["content"][0]["input_audio"]["data"]
                    .as_str()
                    .unwrap()
            )
            .unwrap(),
        WAV
    );
    assert_eq!(messages[2]["content"], "after");
}
// Catches compatible/private adapters inheriting audio solely from a shared JSON shape.
#[test]
fn unproved_profiles_models_and_endpoints_refuse_audio_before_http() {
    use ego_ai::wire::{
        anthropic_messages::AnthropicMessages, gemini_interactions::GeminiInteractions,
        openai_codex_responses::OpenAiCodexResponses, openai_completions::OpenRouterCompletions,
        openai_responses::OpenAiResponses,
    };
    let req = request();
    let cases: Vec<(ProviderRow, Box<dyn WireFormat>)> = vec![
        (
            ProviderRow::openrouter_default(),
            Box::new(OpenRouterCompletions),
        ),
        (
            ProviderRow::anthropic_default(),
            Box::new(AnthropicMessages),
        ),
        (ProviderRow::openai(), Box::new(OpenAiResponses)),
        (
            ProviderRow::openai_codex_default(),
            Box::new(OpenAiCodexResponses),
        ),
        (ProviderRow::gemini_default(), Box::new(GeminiInteractions)),
    ];
    for (row, wire) in cases {
        assert!(
            body(&row, wire.as_ref(), &req)
                .unwrap_err()
                .contains("audio")
        );
    }
    let mut wrong = req.clone();
    wrong.model = "gpt-5.4".into();
    assert!(body(&ProviderRow::openai(), &OpenAiCompletions, &wrong).is_err());
    assert!(
        body(
            &ProviderRow::openai_at("https://example.invalid/v1/".parse().unwrap()),
            &OpenAiCompletions,
            &req
        )
        .is_err()
    );
}
fn caps() -> ModelCapabilities {
    serde_json::from_value(json!({"schema_version":ModelCapabilities::SCHEMA_VERSION,
        "supports_parallel_tool_calls":false,"reasoning_carryover":"unsupported",
        "reasoning_efforts":"any","cancel":"cooperative","cache":"none","transport":"sse",
        "audio_input":"supported_wav"
    }))
    .unwrap()
}
// Catches operator capability overrides promoting an unverified model or endpoint.
#[test]
fn static_and_full_resolution_admit_only_the_exact_published_audio_contract() {
    for (row, model, expected) in [
        (
            ProviderRow::openai(),
            "gpt-audio-1.5",
            AudioInputSupport::SupportedWav,
        ),
        (
            ProviderRow::openai(),
            "gpt-5.4",
            AudioInputSupport::Unsupported,
        ),
        (
            ProviderRow::openai_at("https://example.invalid/v1/".parse().unwrap()),
            "gpt-audio-1.5",
            AudioInputSupport::Unsupported,
        ),
    ] {
        let key = format!("openai/{model}");
        let catalogue = Catalogue::new([row]).with_hosted(
            &key,
            HostedModel {
                capabilities: caps(),
                authority: SuppliedAuthority::System,
                metering: tabulated(),
                rates: None,
            },
        );
        let slug = ModelSlug::parse(&key).unwrap();
        assert_eq!(
            catalogue
                .resolve_hosted(&slug, &OpenAiCompletions)
                .unwrap()
                .capabilities
                .audio_input,
            expected
        );
        assert_eq!(
            catalogue
                .static_request_profile(&slug)
                .unwrap()
                .unwrap()
                .capabilities()
                .audio_input,
            expected
        );
    }
}

// Catches public OpenAI resolution routing the audio-only model through Responses.
#[test]
fn audio_model_static_preflight_uses_chat_completions() {
    let catalogue = Catalogue::new([ProviderRow::openai()]).with_hosted(
        "openai/gpt-audio-1.5",
        HostedModel {
            capabilities: caps(),
            authority: SuppliedAuthority::System,
            metering: tabulated(),
            rates: None,
        },
    );
    let profile = catalogue
        .static_request_profile(&ModelSlug::parse("openai/gpt-audio-1.5").unwrap())
        .unwrap()
        .unwrap();
    profile.ensure_expressible(&request()).unwrap();
}
