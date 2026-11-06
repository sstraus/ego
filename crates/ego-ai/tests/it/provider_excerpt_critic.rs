//! A gateway may echo an upstream Authorization header in its error details.
use ego_ai::http::HttpResponse;
use ego_ai::wire::WireFormat;
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_core::generator::port::AiEvent;
use futures::{StreamExt, stream};

#[tokio::test]
async fn upstream_excerpt_does_not_publish_the_token_after_bearer() {
    // A non-sk bearer is a normal credential shape, including JWTs. Both a
    // header string and a JSON diagnostic can be returned in metadata.raw.
    let token = "eyJhbGciOiJIUzI1NiJ9.criticSyntheticCredential.signature";
    for raw in [
        serde_json::json!(format!("Authorization: Bearer {token}")),
        serde_json::json!({"headers": {"Authorization": format!("Bearer {token}")}}),
    ] {
        let body = serde_json::json!({"error": {
            "message": "Provider returned error",
            "metadata": {"provider_name": "upstream", "raw": raw}
        }})
        .to_string();
        let response = HttpResponse {
            status: 400,
            headers: Vec::new(),
            body: stream::once(async move { Ok(body.into()) }).boxed(),
        };
        let events: Vec<_> = OpenAiCompletions.decode(Ok(response), None).collect().await;
        let diagnostic = events
            .iter()
            .find_map(|event| match event {
                AiEvent::Error { message, .. } => Some(message),
                _ => None,
            })
            .expect("an HTTP failure must report its diagnostic");
        assert!(diagnostic.contains("upstream"));
        assert!(
            !diagnostic.contains(token),
            "upstream excerpt leaked bearer credential: {diagnostic}"
        );
    }
}
