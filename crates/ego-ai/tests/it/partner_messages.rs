//! Claude on Amazon Bedrock and Google Cloud: addresses, credentials and the
//! reasoning-state boundary. The request and stream shapes are pinned by the
//! conformance profiles `bedrock` and `vertex`.

use std::collections::HashMap;

use ego_ai::provider::{
    Credentials, MissingCredential, ProviderRow, Secret, VERTEX_LOCATION_VAR, VERTEX_PROJECT_VAR,
    VertexAddressPart, bedrock_base, vertex_base,
};
use ego_ai::wire::partner_messages::{BEDROCK, VERTEX};
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    ContentBlock, Conversation, GenRequest, Message, ReasoningState, Role, ThinkingLevel,
};
use futures::StreamExt;
use url::Url;

struct Env(HashMap<&'static str, &'static str>);

impl Credentials for Env {
    fn secret(&self, var: &str) -> Result<Secret, MissingCredential> {
        self.0
            .get(var)
            .map(|value| (*value).into())
            .ok_or_else(|| MissingCredential { var: var.into() })
    }
}

fn request(model: &str, history: Vec<Message>) -> GenRequest {
    let mut messages = vec![Message::text(Role::User, "hi")];
    messages.extend(history);
    if messages.len() > 1 {
        messages.push(Message::text(Role::User, "again"));
    }
    GenRequest {
        web_search: None,
        cache: None,
        model: model.to_string(),
        messages: Conversation::new(messages).expect("a valid history"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(16),
        parallel_tool_calls: false,
    }
}

fn reasoning(dialect: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Reasoning {
                state: ReasoningState {
                    blob: r#"{"type":"thinking","thinking":"t","signature":"signed"}"#.to_string(),
                    dialect: dialect.to_string(),
                },
            },
            ContentBlock::Text {
                text: "ok".to_string(),
            },
        ],
    }
}

/// Catches: a host built for the wrong endpoint type, which reaches no Claude
/// deployment, and a value that moves the request to another host.
#[test]
fn addresses_follow_the_documented_host_forms_and_refuse_host_injection() {
    let cases = [
        (
            "global",
            "https://aiplatform.googleapis.com/v1/projects/p-1/locations/global/",
        ),
        (
            "eu",
            "https://aiplatform.eu.rep.googleapis.com/v1/projects/p-1/locations/eu/",
        ),
        (
            "us-east5",
            "https://us-east5-aiplatform.googleapis.com/v1/projects/p-1/locations/us-east5/",
        ),
    ];
    for (location, url) in cases {
        assert_eq!(vertex_base("p-1", location).expect("valid").as_str(), url);
    }
    assert_eq!(
        vertex_base("p-1", "evil.example/x"),
        Err(VertexAddressPart::Location)
    );
    assert_eq!(
        vertex_base("p/../x", "global"),
        Err(VertexAddressPart::Project)
    );
    assert_eq!(
        bedrock_base("eu-west-1").expect("valid").as_str(),
        "https://bedrock-mantle.eu-west-1.api.aws/anthropic/v1/"
    );
    assert!(bedrock_base("evil.example#").is_none());
}

/// Catches: a Vertex credential that resolves without a project and sends
/// every request to an address that names none, and a location that is read
/// but not used.
#[test]
fn a_vertex_credential_needs_a_project_and_carries_its_address() {
    let row = ProviderRow::vertex();
    let missing = row
        .credential
        .resolve(&Env(HashMap::from([("oauth:vertex", "token")])))
        .expect_err("no project is set");
    assert_eq!(missing.var, VERTEX_PROJECT_VAR);
    assert!(
        missing.to_string().contains(VERTEX_PROJECT_VAR),
        "{missing}"
    );

    let no_login = row
        .credential
        .resolve(&Env(HashMap::from([(VERTEX_PROJECT_VAR, "p-1")])))
        .expect_err("no stored token");
    assert_eq!(no_login.var, "ego auth login vertex");

    let resolved = row
        .credential
        .resolve(&Env(HashMap::from([
            (VERTEX_PROJECT_VAR, "p-1"),
            (VERTEX_LOCATION_VAR, "us"),
            ("oauth:vertex", "token"),
        ])))
        .expect("project and token are set");
    assert_eq!(
        resolved.base_override().map(Url::as_str),
        Some("https://aiplatform.us.rep.googleapis.com/v1/projects/p-1/locations/us/")
    );
    let headers = resolved.headers();
    assert_eq!(
        headers
            .iter()
            .map(|header| header.name())
            .collect::<Vec<_>>(),
        ["authorization"]
    );
}

/// Catches: a model id that `Url::join` resolves out of its path segment, so
/// `-m vertex/../../x` reaches another resource of the Google API host.
#[test]
fn a_vertex_model_id_cannot_leave_its_path_segment() {
    let base = Url::parse("https://aiplatform.googleapis.com/v1/projects/p/locations/global/")
        .expect("a URL");
    let target = RequestTarget::new(&base, &[]);
    for model in ["../../x", "a/b", "m?x=1", "m#f", ""] {
        let refused = VERTEX.build_request(&target, &request(model, Vec::new()));
        assert!(refused.is_err(), "{model:?} was accepted");
    }
    let built = VERTEX
        .build_request(&target, &request("claude-haiku-4-5@20251001", Vec::new()))
        .expect("a published id");
    assert_eq!(
        built.url.as_str(),
        "https://aiplatform.googleapis.com/v1/projects/p/locations/global/publishers/anthropic/models/claude-haiku-4-5@20251001:streamRawPredict"
    );
}

/// Catches: a platform's own state sent under its platform tag instead of the
/// Messages dialect the shared projection expects, which the projection
/// refuses. Foreign state is refused earlier, by the capability gate.
#[test]
fn a_platforms_own_reasoning_state_is_replayed() {
    let base = Url::parse("https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/").unwrap();
    let target = RequestTarget::new(&base, &[]);
    let own = BEDROCK
        .build_request(
            &target,
            &request(
                "anthropic.claude-opus-5-5",
                vec![reasoning(BEDROCK.dialect())],
            ),
        )
        .expect("Bedrock replays its own state");
    let body: serde_json::Value = serde_json::from_slice(own.body.as_deref().unwrap()).unwrap();
    assert_eq!(body["messages"][1]["content"][0]["signature"], "signed");
}

/// Catches: decoded state tagged with the first-party dialect, which would let
/// the capability gate replay a Vertex signature to the Anthropic API.
#[tokio::test]
async fn decoded_state_is_tagged_with_the_platform_dialect() {
    let transcript = include_str!("../goldens/profile/vertex/stream/text.sse");
    let chunks = vec![Ok(bytes::Bytes::from_static(transcript.as_bytes()))];
    let events: Vec<AiEvent> = VERTEX
        .decode(
            Ok(ego_ai::http::HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Box::pin(futures::stream::iter(chunks)),
            }),
            None,
        )
        .collect()
        .await;
    let dialects: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AiEvent::ReasoningState(state) => Some(state.dialect.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(dialects, [VERTEX.dialect()], "{events:?}");
}
