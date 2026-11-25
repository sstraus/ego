pub(crate) fn save_instance(
    home: &std::path::Path,
    profile: &str,
    base: &str,
    connection: &str,
    upstream: &str,
    output: u64,
    extra: &str,
) {
    use ego_core::config::{ProviderInstance, ProviderInstanceModel, ProviderModelContract};
    use ego_core::generator::capabilities::{
        AudioInputSupport, CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities,
        ReasoningCarryover, ReasoningEffortSupport, Transport,
    };
    use ego_core::generator::types::ThinkingLevel;
    use std::collections::BTreeMap;

    let model = ProviderInstanceModel {
        upstream_id: upstream.into(),
        contract: ProviderModelContract::ChatStandard,
        facts: ModelCapabilities {
            schema_version: ModelCapabilities::SCHEMA_VERSION,
            max_context_tokens: Some(32000),
            max_output_tokens: Some(output),
            max_tools: Some(8),
            supports_parallel_tool_calls: false,
            reasoning_carryover: ReasoningCarryover::Unsupported,
            reasoning_efforts: ReasoningEffortSupport::Only([ThinkingLevel::Off].into()),
            default_reasoning_effort: Some(ThinkingLevel::Off),
            reasoning_state_dialect: None,
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::None,
            transport: Transport::Sse,
            image_input: ImageInputSupport::Unknown,
            audio_input: AudioInputSupport::Unknown,
        },
        metering: ego_core::budget::MeteringIdentity::Currency {
            settlement: ego_core::budget::SettlementSource::Quote,
        },
    };
    let instance = ProviderInstance {
        profile: profile.parse().unwrap(),
        base_url: base.parse().unwrap(),
        credential_ref: Some("office".parse().unwrap()),
        models: BTreeMap::from([
            ("assistant".parse().unwrap(), model.clone()),
            (extra.parse().unwrap(), model),
        ]),
    };
    let instances = toml::to_string(&BTreeMap::from([(
        "providers",
        BTreeMap::from([("instances", BTreeMap::from([("office", instance)]))]),
    )]))
    .unwrap();
    std::fs::write(
            home.join("config.toml"),
            format!(
                "tools = []\nmodel = 'office/assistant'\nthinking = 'off'\nbudget = '16k'\n[profile.chat]\nmodel = 'office/assistant'\nthinking = 'off'\nsandbox = 'off'\nbudget = '16k'\n\
                 [connections.office]\nname = 'Office'\n{connection}\n{instances}"
            ),
        )
        .unwrap();
}
