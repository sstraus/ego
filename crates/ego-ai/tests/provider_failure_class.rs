//! regression: how a provider-reported failure is classified, and what of
//! it a host may be shown.
//!
//! The live failure was an overloaded Codex backend: its stream carried an
//! `error` event whose code said "overloaded", every Responses mapping
//! recorded it as `Unclassified`, and so nothing above the mapping could ever
//! retry it.

use std::time::Duration;

use ego_ai::http::HttpResponse;
use ego_ai::wire::WireFormat;
use ego_ai::wire::openai_codex_responses::OpenAiCodexResponses;
use ego_ai::wire::openai_responses::OpenAiResponses;
use ego_ai::wire::xai_responses::XaiResponses;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::ErrorKind;
use ego_core::permit::HttpHeader;
use futures::StreamExt;

fn mappings() -> [(&'static str, Box<dyn WireFormat>); 3] {
    [
        ("openai-responses", Box::new(OpenAiResponses)),
        ("xai-responses", Box::new(XaiResponses)),
        ("openai-codex-responses", Box::new(OpenAiCodexResponses)),
    ]
}

async fn decode(
    wire: &dyn WireFormat,
    status: u16,
    headers: Vec<HttpHeader>,
    body: &str,
) -> Vec<AiEvent> {
    wire.decode(
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
    .await
}

fn kinds(events: &[AiEvent]) -> Vec<ErrorKind> {
    events
        .iter()
        .filter_map(|event| match event {
            AiEvent::Error { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

fn causes(events: &[AiEvent]) -> Vec<(String, Option<Duration>)> {
    events
        .iter()
        .filter_map(|event| match event {
            AiEvent::ProviderFailure { cause, retry_after } => Some((cause.clone(), *retry_after)),
            _ => None,
        })
        .collect()
}

/// Every code OpenAI documents for a momentary condition is transient, in
/// all three event shapes a Responses stream can carry it in; an unknown
/// code stays unclassified.
#[tokio::test]
async fn a_streamed_responses_error_is_classified_by_its_code() {
    let shapes = |code: &str| {
        [
            format!(
                "data: {{\"type\":\"error\",\"code\":\"{code}\",\"message\":\"Our servers are currently overloaded. Please try again later.\"}}\n\n"
            ),
            format!(
                "data: {{\"type\":\"error\",\"error\":{{\"code\":\"{code}\",\"message\":\"Our servers are currently overloaded. Please try again later.\"}}}}\n\n"
            ),
            format!(
                "data: {{\"type\":\"response.failed\",\"response\":{{\"status\":\"failed\",\"error\":{{\"code\":\"{code}\",\"message\":\"Our servers are currently overloaded. Please try again later.\"}}}}}}\n\n"
            ),
            format!(
                "data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"failed\",\"error\":{{\"code\":\"{code}\",\"message\":\"Our servers are currently overloaded. Please try again later.\"}}}}}}\n\n"
            ),
        ]
    };
    for (name, wire) in mappings() {
        for (code, expected) in [
            ("server_is_overloaded", ErrorKind::Transient),
            ("rate_limit_exceeded", ErrorKind::Transient),
            ("server_error", ErrorKind::Transient),
            ("insufficient_quota", ErrorKind::Unclassified),
            ("some_future_code", ErrorKind::Unclassified),
        ] {
            for sse in shapes(code) {
                let events = decode(wire.as_ref(), 200, Vec::new(), &sse).await;
                assert_eq!(kinds(&events), [expected], "{name} {code}: {sse}");
                assert_eq!(
                    causes(&events),
                    [(
                        "Our servers are currently overloaded. Please try again later.".to_owned(),
                        None
                    )],
                    "{name} {code}: the provider's typed message is the host cause"
                );
            }
        }
    }
}

/// A non-200 states its own retry delay in seconds, and the cause a host may
/// see is the status and the envelope's typed message — never a body that is
/// not an envelope.
#[tokio::test]
async fn an_error_status_carries_its_retry_after_and_never_relays_a_raw_body() {
    let wire = OpenAiResponses;
    let retry = vec![HttpHeader::new("retry-after", b"7".to_vec()).expect("header")];

    let typed = decode(
        &wire,
        429,
        retry.clone(),
        r#"{"error":{"message":"Rate limit reached","code":"rate_limit_exceeded"}}"#,
    )
    .await;
    assert_eq!(kinds(&typed), [ErrorKind::Transient]);
    assert_eq!(
        causes(&typed),
        [(
            "the provider returned HTTP 429: Rate limit reached (code rate_limit_exceeded)"
                .to_owned(),
            Some(Duration::from_secs(7))
        )]
    );

    const BODY: &str = "<html>proxy secret-session-cookie=abc</html>";
    let raw = decode(&wire, 503, Vec::new(), BODY).await;
    assert_eq!(kinds(&raw), [ErrorKind::Transient]);
    assert_eq!(
        causes(&raw),
        [("the provider returned HTTP 503".to_owned(), None)],
        "a non-envelope body must not reach the host cause"
    );
    assert!(
        !raw.iter().any(|event| matches!(
            event,
            AiEvent::Error { message, .. } if message.contains("secret-session-cookie")
        )),
        "the journal must not keep an untyped body: {raw:?}"
    );

    let unreadable = vec![
        HttpHeader::new("retry-after", b"Wed, 21 Oct 2015 07:28:00 GMT".to_vec()).expect("header"),
    ];
    let dated = decode(&wire, 503, unreadable, "").await;
    assert_eq!(
        causes(&dated),
        [(
            "the provider returned HTTP 503 with an empty body".to_owned(),
            None
        )],
        "an expired HTTP-date Retry-After is not used"
    );
}

#[tokio::test]
async fn all_responses_mappings_retry_sdk_transient_statuses_and_not_invalid_requests() {
    // Catches: 409 conflicts or 529 overloads bypass shared retry classification.
    for (name, wire) in mappings() {
        for status in [408, 409, 429, 500, 503, 529] {
            let events = decode(
                wire.as_ref(),
                status,
                vec![HttpHeader::new("retry-after-ms", b"1250".to_vec()).unwrap()],
                "",
            )
            .await;
            assert_eq!(kinds(&events), [ErrorKind::Transient], "{name} {status}");
            assert_eq!(causes(&events)[0].1, Some(Duration::from_millis(1250)));
        }
        let events = decode(wire.as_ref(), 400, Vec::new(), "").await;
        assert_eq!(kinds(&events), [ErrorKind::Invalid], "{name}");
    }
}

#[tokio::test]
async fn retry_after_date_and_millisecond_precedence_reach_public_provider_failure() {
    // Catches: timing helper support fails to reach the decoder's retry event.
    let now = std::time::SystemTime::now();
    let deadline = now + Duration::from_secs(60);
    let date = httpdate::fmt_http_date(deadline);
    for (name, wire) in mappings() {
        let events = decode(
            wire.as_ref(),
            503,
            vec![HttpHeader::new("retry-after", date.as_bytes().to_vec()).unwrap()],
            "",
        )
        .await;
        let delay = causes(&events)[0].1.expect("HTTP-date reaches retry event");
        let latest = httpdate::parse_http_date(&date)
            .unwrap()
            .duration_since(now)
            .unwrap();
        assert!(
            delay <= latest && delay >= latest.saturating_sub(Duration::from_secs(2)),
            "{name}: {delay:?}"
        );
        let events = decode(
            wire.as_ref(),
            503,
            vec![
                HttpHeader::new("retry-after-ms", b"1250".to_vec()).unwrap(),
                HttpHeader::new("retry-after", date.as_bytes().to_vec()).unwrap(),
            ],
            "",
        )
        .await;
        assert_eq!(
            causes(&events)[0].1,
            Some(Duration::from_millis(1250)),
            "{name}"
        );
        let events = decode(
            wire.as_ref(),
            503,
            vec![HttpHeader::new("retry-after", b"0".to_vec()).unwrap()],
            "",
        )
        .await;
        assert_eq!(
            causes(&events)[0].1,
            None,
            "zero uses local backoff: {name}"
        );
    }
}
