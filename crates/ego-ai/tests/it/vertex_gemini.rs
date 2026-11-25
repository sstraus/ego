//! Offline checks at the configured catalogue and outgoing request boundaries.
use ego_ai::catalogue::{Catalogue, ModelSlug};
use ego_ai::provider::{CredentialStrategy, Credentials, MissingCredential, ProviderRow, Secret};
use ego_ai::wire::RequestTarget;
use ego_ai::wire::gemini_interactions::DIALECT;
use ego_core::config::{
    ConnectionAccess, ConnectionConfig, ProviderInstance, ProviderInstanceModel,
    ProviderModelContract, ProviderProfile, ResolvedProviders, Source, Sourced,
};
use ego_core::generator::capabilities::*;
use ego_core::generator::types::{Conversation, GenRequest, Message, Role, ThinkingLevel};
use std::collections::BTreeMap;
use std::sync::Mutex;

fn instance(base: &str) -> ProviderInstance {
    ProviderInstance {
        profile: ProviderProfile::VertexGemini,
        base_url: base.parse().unwrap(),
        credential_ref: Some("google-work".parse().unwrap()),
        models: BTreeMap::from([(
            "messages-looking-alias".parse().unwrap(),
            ProviderInstanceModel {
                upstream_id: "gemini-4-argon".into(),
                contract: ProviderModelContract::GeminiInteractions,
                facts: ModelCapabilities {
                    schema_version: ModelCapabilities::SCHEMA_VERSION,
                    max_context_tokens: Some(1_048_576),
                    max_output_tokens: Some(65_536),
                    max_tools: Some(8),
                    supports_parallel_tool_calls: false,
                    reasoning_carryover: ReasoningCarryover::SameProviderOnly,
                    reasoning_efforts: ReasoningEffortSupport::Only(
                        [
                            ThinkingLevel::Low,
                            ThinkingLevel::Medium,
                            ThinkingLevel::High,
                        ]
                        .into(),
                    ),
                    default_reasoning_effort: Some(ThinkingLevel::Medium),
                    reasoning_state_dialect: Some(DIALECT.into()),
                    cancel: CancelSemantics::Cooperative,
                    cache: CacheRetention::None,
                    transport: Transport::Sse,
                    image_input: ImageInputSupport::Unknown,
                    audio_input: AudioInputSupport::Unsupported,
                },
                metering: ego_core::budget::MeteringIdentity::Currency {
                    settlement: ego_core::budget::SettlementSource::Quote,
                },
            },
        )]),
    }
}
fn providers(spec: ProviderInstance) -> ResolvedProviders {
    ResolvedProviders {
        instances: BTreeMap::from([(
            "office-google".parse().unwrap(),
            Sourced {
                source: Source::User,
                value: spec,
            },
        )]),
    }
}
fn connections() -> BTreeMap<String, ConnectionConfig> {
    BTreeMap::from([(
        "google-work".into(),
        ConnectionConfig {
            name: "Work Google".into(),
            vendor: "vertex-gemini".into(),
            access: ConnectionAccess::Oauth,
            key_env: None,
            azure_resource: None,
        },
    )])
}
struct ScopedSecrets(Mutex<Vec<String>>);
impl Credentials for ScopedSecrets {
    fn secret(&self, name: &str) -> Result<Secret, MissingCredential> {
        self.0.lock().unwrap().push(name.into());
        if name == "oauth:vertex-gemini" {
            Ok("scoped-google-token".into())
        } else {
            Err(MissingCredential { var: name.into() })
        }
    }
}

// Catches: Gemini retargets Claude, invents default models/prices, or a key
// connection masquerades as cloud OAuth.
#[test]
fn vertex_gemini_is_configured_oauth_only_and_keeps_claude_row() {
    let rows = ego_ai::conformance::default_final_provider_rows().unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row.id() == "vertex-gemini")
            .count(),
        1
    );
    let catalogue = Catalogue::new(rows);
    let gemini = catalogue.row("vertex-gemini").unwrap();
    assert!(
        matches!(&gemini.credential, CredentialStrategy::OAuth { provider }
        if provider == "vertex-gemini")
    );
    assert!(gemini.price_table_name().is_none());
    assert!(gemini.cost.is_none());
    let slug = ModelSlug::parse("vertex-gemini/gemini-4-argon").unwrap();
    assert!(catalogue.static_request_profile(&slug).is_err());
    let claude = catalogue.row("vertex").unwrap();
    assert!(
        matches!(&claude.credential, CredentialStrategy::GoogleCloud { provider, .. }
        if provider == "vertex")
    );
    assert_eq!(
        claude.price_table_name(),
        Some(("vertex_ai-anthropic_models", "vertex_ai"))
    );
    let spec =
        instance("https://aiplatform.googleapis.com/v1/projects/work-project/locations/global/");
    for (vendor, access, key_env) in [
        ("vertex", ConnectionAccess::Oauth, None),
        ("gemini", ConnectionAccess::Oauth, None),
        (
            "vertex-gemini",
            ConnectionAccess::Key,
            Some("GOOGLE_TEST_API_KEY"),
        ),
    ] {
        let mut accounts = connections();
        let entry = accounts.get_mut("google-work").unwrap();
        entry.vendor = vendor.into();
        entry.access = access;
        entry.key_env = key_env.map(str::to_owned);
        assert!(
            Catalogue::new([ProviderRow::vertex_gemini()])
                .with_instances(&providers(spec.clone()), &accounts)
                .is_err()
        );
    }
    assert!(
        Catalogue::new([ProviderRow::vertex_gemini()])
            .with_instances(&providers(spec), &BTreeMap::new())
            .is_err()
    );
}

// Catches: project/location/host disagreement publishes before credentials;
// unsupported model facts advertise long decode or minimal thinking.
#[test]
fn vertex_gemini_route_and_foreground_facts_fail_before_publication() {
    for base in [
        "https://aiplatform.googleapis.com/v1/projects/work-project/locations/us/",
        "https://aiplatform.us.rep.googleapis.com/v1/projects/work-project/locations/eu/",
        "https://us-central1-aiplatform.googleapis.com/v1/projects/work-project/locations/us-central1/",
        "https://aiplatform.googleapis.com/v1/projects//locations/global/",
        "https://aiplatform.googleapis.com/v1/projects/work%2Fother/locations/global/",
        "https://aiplatform.googleapis.com:8443/v1/projects/work-project/locations/global/",
        "https://attacker.example/v1/projects/work-project/locations/global/",
        "https://aiplatform.googleapis.com/v1/projects/work-project/locations/global/interactions/",
    ] {
        let spec = instance(base);
        assert!(spec.validate().unwrap_err().contains("vertex-gemini"));
        assert!(
            Catalogue::new([ProviderRow::vertex_gemini()])
                .with_instances(&providers(spec), &connections())
                .is_err()
        );
    }
    let valid =
        instance("https://aiplatform.googleapis.com/v1/projects/work-project/locations/global/");
    let mut missing_reference = valid.clone();
    missing_reference.credential_ref = None;
    assert!(missing_reference.validate().is_err());
    for change in 0..12 {
        let mut invalid = valid.clone();
        let model = invalid.models.values_mut().next().unwrap();
        match change {
            0 => model.upstream_id = "unreviewed-model".into(),
            1 => model.facts.max_output_tokens = Some(65_537),
            2 => model.facts.max_context_tokens = Some(1_048_577),
            3 => model.facts.reasoning_efforts = ReasoningEffortSupport::Any,
            4 => model.facts.default_reasoning_effort = Some(ThinkingLevel::Minimal),
            5 => model.facts.reasoning_state_dialect = Some("messages".into()),
            6 => model.facts.reasoning_carryover = ReasoningCarryover::Portable,
            7 => model.facts.transport = Transport::WebSocket,
            8 => model.facts.cache = CacheRetention::Long,
            9 => model.facts.audio_input = AudioInputSupport::SupportedWav,
            10 => model.facts.max_tools = None,
            _ => model.contract = ProviderModelContract::Messages,
        }
        assert!(invalid.validate().is_err(), "change {change}");
    }
    let decoded: ProviderInstance =
        serde_json::from_value(serde_json::to_value(&valid).unwrap()).unwrap();
    assert_eq!(decoded, valid);
}

// Catches: a configured deployment uses the Developer API or Claude Messages
// contract, sends a key instead of OAuth, or loses the configured authority.
#[test]
fn vertex_gemini_sends_v1_stateless_interactions_to_its_configured_project() {
    for (location, host) in [
        ("global", "aiplatform.googleapis.com"),
        ("us", "aiplatform.us.rep.googleapis.com"),
        ("eu", "aiplatform.eu.rep.googleapis.com"),
    ] {
        let base = format!("https://{host}/v1/projects/work-project/locations/{location}/");
        let catalogue = Catalogue::new([ProviderRow::vertex_gemini()])
            .with_instances(&providers(instance(&base)), &connections())
            .unwrap();
        let slug = ModelSlug::parse("office-google/messages-looking-alias").unwrap();
        let profile = catalogue.static_request_profile(&slug).unwrap().unwrap();
        let secrets = ScopedSecrets(Mutex::new(Vec::new()));
        let credential = catalogue
            .row("office-google")
            .unwrap()
            .credential
            .resolve(&secrets)
            .unwrap();
        assert!(credential.base_override().is_none());
        assert_eq!(*secrets.0.lock().unwrap(), ["oauth:vertex-gemini"]);
        let (http, unasked) = crate::support::unasked();
        let resolved =
            futures::executor::block_on(catalogue.resolve(&slug, &http, &credential)).unwrap();
        assert_eq!(resolved.model, "gemini-4-argon");
        assert_eq!(profile.request_model(), resolved.model);
        assert_eq!(profile.capabilities(), &resolved.capabilities);
        assert!(unasked.asked().is_empty());
        let request = GenRequest {
            model: resolved.model.clone(),
            messages: Conversation::new(vec![Message::text(Role::User, "Summarize the report")])
                .unwrap(),
            tools: Vec::new(),
            thinking: ThinkingLevel::Medium,
            max_output_tokens: Some(2048),
            parallel_tool_calls: false,
            web_search: None,
            cache: None,
        };
        profile.ensure_expressible(&request).unwrap();
        let built = resolved
            .wire
            .build_request(
                &RequestTarget::new(resolved.row.base(), &credential.headers()),
                &request,
            )
            .unwrap();
        assert_eq!(built.url.as_str(), format!("{base}interactions"));
        assert!(
            built
                .headers
                .iter()
                .any(|header| header.name() == "authorization"
                    && header.value() == b"Bearer scoped-google-token")
        );
        assert!(
            built
                .headers
                .iter()
                .any(|header| header.name() == "accept" && header.value() == b"text/event-stream")
        );
        assert!(
            !built
                .headers
                .iter()
                .any(|header| header.name() == "x-goog-api-key")
        );
        let body: serde_json::Value =
            serde_json::from_slice(built.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["model"], "gemini-4-argon");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(
            body["input"],
            serde_json::json!([{
                "type": "user_input",
                "content": [{"type": "text", "text": "Summarize the report"}]
            }])
        );
    }
}

mod headers;
