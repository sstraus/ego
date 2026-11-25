//! Story 921-010d: an exhausted account quota is not a rate limit.
//!
//! Providers send both as HTTP 429 (Anthropic also as 400 for an operator
//! spend limit). Only the typed body tells them apart, and a quota refusal
//! never clears by waiting. Fixtures are derived from provider documents and
//! the provider's own client; `tests/fixtures/quota/README.md` states which.

use ego_ai::http::HttpResponse;
use ego_ai::wire::WireFormat;
use ego_ai::wire::anthropic_messages::AnthropicMessages;
use ego_ai::wire::gemini_interactions::GeminiInteractions;
use ego_ai::wire::openai_codex_responses::OpenAiCodexResponses;
use ego_ai::wire::openai_responses::OpenAiResponses;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::ErrorKind;
use ego_core::permit::HttpHeader;
use futures::StreamExt;
use serde_json::Value;

const FIXTURES: [&str; 4] = [
    include_str!("../fixtures/quota/openai.json"),
    include_str!("../fixtures/quota/anthropic.json"),
    include_str!("../fixtures/quota/gemini.json"),
    include_str!("../fixtures/quota/chatgpt-subscription.json"),
];

fn wire(name: &str) -> Box<dyn WireFormat> {
    match name {
        "openai-responses" => Box::new(OpenAiResponses),
        "anthropic-messages" => Box::new(AnthropicMessages),
        "gemini-interactions" => Box::new(GeminiInteractions),
        "openai-codex-responses" => Box::new(OpenAiCodexResponses),
        other => panic!("fixture names an unknown wire {other}"),
    }
}

/// The class and capacity flag one non-200 response decodes to.
async fn classify(
    wire: &dyn WireFormat,
    status: u16,
    retry_after: Option<u64>,
    body: &str,
) -> (ErrorKind, bool, String) {
    let headers = retry_after
        .map(|secs| {
            vec![HttpHeader::new("retry-after", secs.to_string().into_bytes()).expect("header")]
        })
        .unwrap_or_default();
    let events: Vec<AiEvent> = wire
        .decode(
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(futures::stream::iter(vec![Ok(
                    bytes::Bytes::copy_from_slice(body.as_bytes()),
                )])),
            }),
            None,
        )
        .collect()
        .await;
    let kind = events.iter().find_map(|event| match event {
        AiEvent::Error { kind, .. } => Some(*kind),
        _ => None,
    });
    let failure = events.iter().find_map(|event| match event {
        AiEvent::ProviderFailure {
            capacity, cause, ..
        } => Some((*capacity, cause.clone())),
        _ => None,
    });
    let (capacity, cause) = failure.expect("a non-200 reports a provider failure");
    (kind.expect("a non-200 reports an error"), capacity, cause)
}

/// Every documented quota, credit or spend refusal is `Refused` and not a
/// capacity refusal; every documented rate limit stays a transient capacity
/// refusal; an ordinary bad request stays `Invalid`.
///
/// Catches: non-200 classification reads only the status, so a quota 429 is
/// retried as capacity (OpenAI codes, Gemini `quota_exceeded`, the ChatGPT
/// subscription limit); a shared `rate_limit_error` type taken as proof of a
/// rate limit (Anthropic spend cap); an operator spend limit reported as a
/// malformed request (Anthropic 400).
#[tokio::test]
async fn documented_quota_refusals_are_refused_and_rate_limits_stay_transient() {
    for fixture in FIXTURES {
        let fixture: Value = serde_json::from_str(fixture).expect("fixture JSON");
        let wire = wire(fixture["wire"].as_str().expect("wire"));
        for case in fixture["cases"].as_array().expect("cases") {
            let name = case["name"].as_str().expect("name");
            let status = u16::try_from(case["status"].as_u64().expect("status")).expect("u16");
            let body = serde_json::to_string(&case["body"]).expect("body");
            let (kind, capacity, cause) =
                classify(wire.as_ref(), status, case["retry_after"].as_u64(), &body).await;
            let expected = match case["expect"].as_str().expect("expect") {
                "refused" => (ErrorKind::Refused, false),
                "transient" => (ErrorKind::Transient, true),
                "invalid" => (ErrorKind::Invalid, false),
                other => panic!("unknown expectation {other}"),
            };
            assert_eq!(
                (kind, capacity),
                expected,
                "{}: {name}: {cause}",
                fixture["wire"]
            );
            if expected.0 == ErrorKind::Refused && status != 402 {
                assert!(
                    cause.contains("retrying will not help"),
                    "{name}: a quota refusal names the account cause: {cause}"
                );
            }
        }
    }
}

/// An unknown code or a body that is not an envelope keeps the status-only
/// class: a 429 stays a transient capacity refusal.
///
/// Catches: quota classification guessed from the status or from an unknown
/// code, which would stop retries of a genuine rate limit.
#[tokio::test]
async fn an_unknown_or_malformed_429_keeps_the_status_class() {
    for (name, wire) in [
        (
            "openai-responses",
            Box::new(OpenAiResponses) as Box<dyn WireFormat>,
        ),
        ("anthropic-messages", Box::new(AnthropicMessages)),
    ] {
        for body in [
            r#"{"error":{"message":"Something new","code":"some_future_code"}}"#,
            "<html>gateway said no</html>",
            "",
        ] {
            let (kind, capacity, cause) = classify(wire.as_ref(), 429, None, body).await;
            assert_eq!(
                (kind, capacity),
                (ErrorKind::Transient, true),
                "{name}: {body}: {cause}"
            );
        }
    }
}
