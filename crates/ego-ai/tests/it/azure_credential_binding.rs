//! Attack table: an Azure connection's credential reaches only its own resource.

use std::collections::BTreeMap;

use ego_ai::catalogue::Catalogue;
use ego_ai::provider::{CredentialStrategy, ProviderRow};
use ego_core::config::{ConnectionConfig, ProviderInstance, ResolvedProviders, Source, Sourced};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};

const A: &str = "https://a.openai.azure.com/openai/v1/";
const B: &str = "https://b.openai.azure.com/openai/v1/";

fn caps() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
        max_context_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
        max_tools: None,
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::SameProviderOnly,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Long,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
    }
}

fn instance(profile: &str, base: &str, reference: &str) -> ProviderInstance {
    serde_json::from_value(serde_json::json!({
        "profile": profile, "base_url": base, "credential_ref": reference,
        "models": {"m": {
            "upstream_id": "dep", "contract": "chat-standard",
            "facts": serde_json::to_value(caps()).unwrap(),
            "metering": serde_json::to_value(ego_core::budget::MeteringIdentity::Currency {
                settlement: ego_core::budget::SettlementSource::Quote,
            }).unwrap()
        }}
    }))
    .expect("instance fixture parses")
}

fn conn(json: serde_json::Value) -> ConnectionConfig {
    serde_json::from_value(json).unwrap()
}

fn build(
    instances: &[(&str, ProviderInstance)],
    connections: &BTreeMap<String, ConnectionConfig>,
) -> Result<Catalogue, String> {
    let providers = ResolvedProviders {
        instances: instances
            .iter()
            .map(|(id, value)| {
                (
                    id.parse().unwrap(),
                    Sourced {
                        source: Source::User,
                        value: value.clone(),
                    },
                )
            })
            .collect(),
    };
    Catalogue::new([ProviderRow::openai()]).with_instances(&providers, connections)
}

// Catches: one resource's key or token handed to another resource, another vendor's
// instance (foreign host) or a plain OpenAI instance through a shared connection.
#[test]
fn azure_credentials_never_cross_resource_or_vendor() {
    let connections = BTreeMap::from([
        (
            "ka".to_owned(),
            conn(
                serde_json::json!({"name":"A","vendor":"azure-openai","access":"key",
                "key_env":"A_API_KEY","azure_resource":A}),
            ),
        ),
        (
            "kb".to_owned(),
            conn(
                serde_json::json!({"name":"B","vendor":"azure-openai","access":"key",
                "key_env":"B_API_KEY","azure_resource":B}),
            ),
        ),
        (
            "oa".to_owned(),
            conn(
                serde_json::json!({"name":"OA","vendor":"azure-openai","access":"oauth",
                "azure_resource":A}),
            ),
        ),
        (
            "plain".to_owned(),
            conn(
                serde_json::json!({"name":"P","vendor":"openai","access":"key",
                "key_env":"PLAIN_API_KEY"}),
            ),
        ),
    ]);
    // Each deployment gets only its own slot.
    let ok = build(
        &[
            ("da", instance("azure-openai", A, "ka")),
            ("db", instance("azure-openai", B, "kb")),
            ("dt", instance("azure-openai", A, "oa")),
        ],
        &connections,
    )
    .expect("matching bindings admit");
    for (id, var) in [("da", "A_API_KEY"), ("db", "B_API_KEY")] {
        assert_eq!(ok.row(id).unwrap().credential.environment_var(), Some(var));
    }
    assert!(matches!(
        &ok.row("dt").unwrap().credential,
        CredentialStrategy::OAuth { provider } if provider == "azure-openai"
    ));
    for (what, bad) in [
        ("key of A on B", instance("azure-openai", B, "ka")),
        ("token of A on B", instance("azure-openai", B, "oa")),
        (
            "plain OpenAI key on Azure",
            instance("azure-openai", A, "plain"),
        ),
        (
            "Azure key on a foreign host",
            instance("openai-compatible", "https://evil.example/v1/", "ka"),
        ),
        (
            "Azure token on a foreign host",
            instance("openai-compatible", "https://evil.example/v1/", "oa"),
        ),
        (
            "Azure key on an OpenAI instance",
            instance("openai", "https://api.openai.com/v1/", "ka"),
        ),
    ] {
        assert!(build(&[("x", bad)], &connections).is_err(), "{what}");
    }
}
