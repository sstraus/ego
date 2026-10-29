//! The public OpenAI Responses mapping, and the Chat endpoint it replaced.
//!
//! `docs/guides/providers.md`
//! asks for two different things and this file proves both. One is that the
//! public row speaks the public Responses contract. The other is negative and
//! is the reason the ADR was written: that **nothing** of the private Codex
//! backend, and nothing of the Chat mapping this row used to select, survives
//! on it. A positive test alone would pass just as happily against a body that
//! also carried a Codex identity.

use base64::Engine;
use ego_ai::http::HttpResponse;
use ego_ai::provider::ProviderRow;
use ego_ai::wire::openai_responses::{self, Inexpressible, OpenAiResponses};
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, GenRequest, Image, ImageMediaType, Message, ReasoningState, Role,
    ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::HttpHeader;
use futures::StreamExt;
use serde_json::Value;

/// A one-pixel PNG, because `Image::new` proves the dimensions from the bytes
/// rather than taking anybody's word for them.
const IMAGE_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn png() -> Image {
    Image::new(
        base64::engine::general_purpose::STANDARD
            .decode(IMAGE_PNG)
            .expect("the fixture is base64"),
        ImageMediaType::Png,
    )
    .expect("the fixture is a valid PNG")
}

fn ask(thinking: ThinkingLevel) -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: "gpt-5.6-sol".to_string(),
        messages: Conversation::new(vec![Message::text(Role::User, "Reply with: ok")])
            .expect("one user message is a conversation"),
        tools: Vec::new(),
        thinking,
        max_output_tokens: Some(16),
        parallel_tool_calls: false,
    }
}

fn body(req: &GenRequest) -> Value {
    serde_json::from_slice(
        &openai_responses::request_body(req).expect("the request is expressible"),
    )
    .expect("the body a mapping built is JSON")
}

/// The endpoint, and the one it must never be again.
///
/// Written as the whole URL rather than as base-plus-path, because the claim
/// is about the address a request goes to. `chat/completions` is still a real
/// path this build joins — four other rows delegate to the Chat mapping — so
/// the guard is that the *public OpenAI row* cannot reach it, not that the
/// string is gone from the workspace.
#[test]
fn the_public_row_posts_to_responses_and_can_no_longer_reach_chat_completions() {
    let row = ProviderRow::openai();
    let request = OpenAiResponses
        .build_request(
            &RequestTarget::new(row.base(), &[]),
            &ask(ThinkingLevel::Off),
        )
        .expect("the canonical request is expressible");
    assert_eq!(request.url.as_str(), "https://api.openai.com/v1/responses");
    assert!(
        !request.url.as_str().contains("chat/completions"),
        "the public row is back on Chat Completions: {}",
        request.url
    );
}

/// Retention is opted out of on every request, not left to the default.
///
/// The Responses API defaults `store` to `true`. A harness that governs what
/// leaves a machine does not let a default decide that a copy of every prompt
/// stays with the provider, so this is pinned rather than assumed — and it is
/// pinned as an explicit `false` in the body, because omitting the field would
/// silently mean `true`.
#[test]
fn store_is_explicitly_false_rather_than_omitted() {
    let body = body(&ask(ThinkingLevel::Off));
    assert_eq!(
        body.get("store"),
        Some(&Value::Bool(false)),
        "store must be present and false: {body}"
    );
}

/// Nothing private to the Codex backend appears in a public request.
///
/// The named fields and headers are the ones `openai_codex_responses` and the
/// `openai-codex` row really carry, so this is a guard against the concrete
/// leak the ADR describes rather than a spell-check: "Reuse private Codex
/// Responses — imports private endpoint, identity, and body semantics into a
/// public API."
#[test]
fn no_codex_private_bytes_reach_the_public_request() {
    let row = ProviderRow::openai();
    let request = OpenAiResponses
        .build_request(
            &RequestTarget::new(row.base(), &[]),
            &ask(ThinkingLevel::Off),
        )
        .expect("the canonical request is expressible");
    let names: Vec<&str> = request.headers.iter().map(HttpHeader::name).collect();
    for private in [
        "originator",
        "chatgpt-account-id",
        "session_id",
        "openai-beta",
    ] {
        assert!(
            !names.contains(&private),
            "the public row sent the Codex-private header `{private}`: {names:?}"
        );
    }
    assert!(
        row.extra_headers.is_empty(),
        "the public row declares an identity it should not: {:?}",
        row.extra_headers
    );

    let body = body(&ask(ThinkingLevel::Off));
    let object = body.as_object().expect("the body is an object");
    for private in [
        "instructions",
        "session_id",
        "previous_response_id",
        "include",
    ] {
        assert!(
            !object.contains_key(private),
            "the public body carries `{private}`, which is Codex's or unasked for: {body}"
        );
    }
    // Chat Completions spellings, which would mean the swap was partial.
    for chat in [
        "messages",
        "max_completion_tokens",
        "reasoning_effort",
        "stream_options",
    ] {
        assert!(
            !object.contains_key(chat),
            "the public body still speaks Chat Completions (`{chat}`): {body}"
        );
    }
}

/// Every level ego can ask for has a spelling, and none of them is invented.
///
/// The mapping is total on purpose — which model accepts which level is a
/// per-model fact `ModelCapabilities::reasoning_efforts` already holds and
/// refuses on, before a request is built. This pins the *spellings*, which are
/// the mapping's own business, and would fail if a level silently mapped to a
/// neighbouring one.
#[test]
fn every_thinking_level_maps_to_its_own_openai_spelling() {
    let expected = [
        (ThinkingLevel::Off, "none"),
        (ThinkingLevel::Minimal, "minimal"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::XHigh, "xhigh"),
        (ThinkingLevel::Max, "max"),
    ];
    let mut seen = Vec::new();
    for (level, spelling) in expected {
        let body = body(&ask(level));
        let effort = body["reasoning"]["effort"]
            .as_str()
            .expect("reasoning.effort is a string")
            .to_string();
        assert_eq!(effort, spelling, "{level:?} sent the wrong effort");
        seen.push(effort);
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 7, "two levels collapsed onto one spelling");
}

/// A tool becomes a flat function tool, not a Chat-style nested one.
#[test]
fn tools_are_flat_function_entries_with_parallel_tool_calls_stated() {
    let mut req = ask(ThinkingLevel::Low);
    req.tools = vec![ToolDescriptor {
        name: "read_file".to_string(),
        description: "Read a file".to_string(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }];
    req.parallel_tool_calls = true;
    let body = body(&req);
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert!(
        body["tools"][0].get("function").is_none(),
        "that is the Chat Completions nesting: {body}"
    );
    assert_eq!(body["parallel_tool_calls"], Value::Bool(true));
}

/// With no tools offered, `parallel_tool_calls` is absent rather than false.
///
/// Sending `false` would be a claim about a request that offers nothing to
/// call in parallel — a field with no referent, which is the kind of noise
/// that makes a diff against a vendor's example unreadable.
#[test]
fn parallel_tool_calls_is_omitted_when_no_tool_is_offered() {
    let body = body(&ask(ThinkingLevel::Off));
    assert!(
        body.as_object()
            .expect("an object")
            .get("parallel_tool_calls")
            .is_none(),
        "an untooled request stated parallel_tool_calls: {body}"
    );
}

/// Reasoning state is refused rather than dropped.
///
/// Dropping it would silently downgrade a continuation, which is why the
/// mapping refuses instead: the caller finds out at the request boundary
/// rather than through a worse answer.
#[test]
fn carried_reasoning_state_is_refused_by_name() {
    let mut req = ask(ThinkingLevel::High);
    req.messages = Conversation::new(vec![Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Reasoning {
            state: ReasoningState {
                blob: "opaque".to_string(),
                dialect: "openai-responses".to_string(),
            },
        }],
    }])
    .expect("one assistant message is a conversation");
    assert_eq!(
        openai_responses::request_body(&req),
        Err(Inexpressible::ReasoningStateNotCarriable)
    );
}

/// Images ride as `input_image` data URLs, before the text of their turn.
#[test]
fn a_user_image_becomes_an_input_image_before_the_text() {
    let mut req = ask(ThinkingLevel::Off);
    req.messages = Conversation::new(vec![Message {
        role: Role::User,
        content: vec![
            ContentBlock::Image { image: png() },
            ContentBlock::Text {
                text: "what is this".to_string(),
            },
        ],
    }])
    .expect("one user message is a conversation");
    let body = body(&req);
    let content = &body["input"][0]["content"];
    assert_eq!(content[0]["type"], "input_image");
    assert!(
        content[0]["image_url"]
            .as_str()
            .expect("a data url")
            .starts_with("data:image/png;base64,"),
        "{content}"
    );
    assert_eq!(content[1]["type"], "input_text");
    assert_eq!(content[1]["text"], "what is this");
}

/// An image outside a user turn is refused rather than relocated — and it is
/// refused before a mapping is ever reached.
///
/// The refusal belongs to [`Conversation`], which will not construct a history
/// with an image in an assistant turn at all, so `Inexpressible::
/// ImageRoleNotSupported` is unreachable through this door. The mapping keeps
/// the arm anyway, exactly as `xai_responses` does: it is the arm that makes
/// the `match` exhaustive, and an arm that silently relocated the image would
/// be the failure worth guarding against if a second door were ever added.
/// What this test can honestly assert is where the refusal actually happens.
#[test]
fn an_image_in_an_assistant_turn_is_refused_before_a_mapping_sees_it() {
    let refused = Conversation::new(vec![Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Image { image: png() }],
    }])
    .expect_err("an assistant turn may not carry an image");
    assert!(
        refused.to_string().contains("only user messages"),
        "the refusal does not say why: {refused}"
    );
}

/// Chunked at an awkward width on purpose: an SSE frame split mid-event is
/// the ordinary case on a real socket, and a decoder that only works on whole
/// frames passes a test that never splits one.
async fn events(sse: &str) -> Vec<AiEvent> {
    OpenAiResponses
        .decode(
            Ok(HttpResponse {
                status: 200,
                headers: vec![
                    HttpHeader::new("content-type", b"text/event-stream".to_vec()).expect("header"),
                ],
                body: Box::pin(futures::stream::iter(
                    sse.as_bytes()
                        .chunks(11)
                        .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
                        .collect::<Vec<_>>(),
                )),
            }),
            None,
        )
        .collect()
        .await
}

/// Both cache figures survive decoding, and neither is inferred from the other.
///
/// `cached_tokens` is what was read from cache and `cache_write_tokens` is what
/// was written into it. The uncached figure subtracts only the first: the
/// vendor does not say written tokens are excluded from `input_tokens`, and a
/// usage line assembled from a guess is not evidence.
#[tokio::test]
async fn the_three_input_parts_partition_the_stated_input_total() {
    let sse = concat!(
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",",
        "\"usage\":{\"input_tokens\":10,\"input_tokens_details\":{\"cached_tokens\":3,",
        "\"cache_write_tokens\":2},\"output_tokens\":3,",
        "\"output_tokens_details\":{\"reasoning_tokens\":1}}}}\n\n",
        "data: [DONE]\n\n"
    );
    let usage = events(sse)
        .await
        .into_iter()
        .find_map(|event| match event {
            AiEvent::Usage(usage) => Some(usage),
            _ => None,
        })
        .expect("the stream reported usage");
    assert_eq!(
        usage.uncached_input_tokens, 5,
        "10 input minus 3 read minus 2 written"
    );
    assert_eq!(usage.cache_read_tokens, 3);
    assert_eq!(usage.cache_write_tokens, 2);
    assert_eq!(
        usage.uncached_input_tokens + usage.cache_read_tokens + usage.cache_write_tokens,
        10,
        "the three input parts must partition the stated input total"
    );
    assert_eq!(usage.output_tokens, 3);
    assert_eq!(usage.reasoning_tokens, Some(1));
}

/// A cached count above the input total is discarded, not saturated.
///
/// Saturating would bill a caller for tokens under a name that never
/// occurred, which is worse than reporting nothing and saying so.
#[tokio::test]
async fn incoherent_usage_is_discarded_with_an_error_rather_than_saturated() {
    let sse = concat!(
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",",
        "\"usage\":{\"input_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":9},",
        "\"output_tokens\":1}}}\n\n",
        "data: [DONE]\n\n"
    );
    let events = events(sse).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AiEvent::Usage(_))),
        "an incoherent usage line was reported anyway: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AiEvent::Error { message, .. }
                if message.contains("cached input count")
        )),
        "the discard was silent: {events:?}"
    );
}
