//! contract for regression case: one ordered, versioned model snapshot.

use ego_ai::catalogue::{
    CachedDiscoveryModel, CachedDiscoverySlice, DiscoveryCacheV2, FactFreshness, ModelAvailability,
    ModelCatalogueEntry, ModelCatalogueSnapshot, ModelCatalogueSourceStatus, ModelDiscoveryEntry,
    ModelDiscoveryFailure, ModelDiscoveryReport, ModelDiscoverySource, ModelFactProvenance,
    ModelSlug, SuppliedAuthority,
};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::public::MeteringKind;
use futures::executor::block_on;

mod support;
use support::{server, unasked};

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        max_context_tokens: None,
        max_output_tokens: None,
        max_tools: None,
        supports_parallel_tool_calls: false,
        reasoning_carryover: ReasoningCarryover::Unsupported,
        reasoning_efforts: ReasoningEffortSupport::Unknown,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::None,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
    }
}

fn row(slug: &str, provenance: ModelFactProvenance, metering: MeteringKind) -> ModelCatalogueEntry {
    ModelCatalogueEntry {
        slug: ModelSlug::parse(slug).unwrap(),
        capabilities: capabilities(),
        metering,
        provenance,
        freshness: FactFreshness::Cached,
        availability: ModelAvailability::Unavailable {
            cause: "source_unreachable".parse().unwrap(),
            message: "source unavailable".into(),
        },
    }
}

#[test]
fn snapshot_is_v2_json_with_explicit_nulls_empty_arrays_and_full_slug_order() {
    let snapshot = ModelCatalogueSnapshot::from_entries_and_sources(
        vec![
            row(
                "zeta/z-model",
                ModelFactProvenance::discovered("zeta"),
                MeteringKind::Unmetered,
            ),
            row(
                "openai/gpt-5.6",
                ModelFactProvenance::supplied(SuppliedAuthority::User),
                MeteringKind::Currency,
            ),
        ],
        vec![ModelCatalogueSourceStatus::unavailable(
            "zeta",
            ModelFactProvenance::discovered("zeta"),
            FactFreshness::Cached,
            "source_unreachable",
            "daemon unavailable",
        )],
    );
    assert_eq!(
        snapshot
            .models
            .iter()
            .map(|m| m.slug.to_string())
            .collect::<Vec<_>>(),
        ["openai/gpt-5.6", "zeta/z-model"]
    );
    let json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "v": 2,
            "models": [
                {
                    "slug": "openai/gpt-5.6",
                    "capabilities": serde_json::to_value(capabilities()).unwrap(),
                    "metering": "currency",
                    "standard_token_price_multiplier": 1,
                    "fast": null,
                    "provenance": { "source": "supplied", "authority": "user" },
                    "freshness": "cached",
                    "availability": {
                        "status": "unavailable",
                        "cause": "source_unreachable",
                        "message": "source unavailable"
                    }
                },
                {
                    "slug": "zeta/z-model",
                    "capabilities": serde_json::to_value(capabilities()).unwrap(),
                    "metering": "unmetered",
                    "standard_token_price_multiplier": 1,
                    "fast": null,
                    "provenance": { "source": "discovered", "provider": "zeta" },
                    "freshness": "cached",
                    "availability": {
                        "status": "unavailable",
                        "cause": "source_unreachable",
                        "message": "source unavailable"
                    }
                }
            ],
            "sources": [
                {
                    "provider": "zeta",
                    "provenance": { "source": "discovered", "provider": "zeta" },
                    "freshness": "cached",
                    "availability": {
                        "status": "unavailable",
                        "cause": "source_unreachable",
                        "message": "daemon unavailable"
                    }
                }
            ]
        })
    );
}

#[test]
fn provenance_freshness_and_availability_are_independent_and_unknown_facts_stay_unknown() {
    let entry = row(
        "ollama/gemma4:12b",
        ModelFactProvenance::discovered("ollama"),
        MeteringKind::Unmetered,
    );
    assert_eq!(entry.capabilities.max_context_tokens, None);
    assert_eq!(entry.capabilities.max_output_tokens, None);
    assert!(matches!(
        entry.provenance,
        ModelFactProvenance::Discovered { .. }
    ));
    assert_eq!(entry.freshness, FactFreshness::Cached);
    assert!(matches!(
        entry.availability,
        ModelAvailability::Unavailable { .. }
    ));
}

#[test]
fn discovery_report_can_be_partial_with_named_and_unnamed_failures() {
    let report = ModelDiscoveryReport::try_new(
        ModelDiscoverySource::Discovered {
            provider: "ollama".into(),
        },
        vec![],
        vec![
            ModelDiscoveryFailure::named("broken", "source_malformed", "invalid model facts"),
            ModelDiscoveryFailure::source("source_unreachable", "connection refused"),
        ],
        false,
    )
    .unwrap();
    assert!(!report.is_complete());
    assert_eq!(report.failures()[0].model.as_deref(), Some("broken"));
    assert_eq!(report.failures()[1].model, None);
    assert!(
        report
            .failures()
            .iter()
            .any(|failure| failure.model.is_none()),
        "an incomplete report must carry an unnamed source failure"
    );
}

#[test]
fn incomplete_discovery_with_only_named_failures_is_rejected() {
    assert!(
        ModelDiscoveryReport::try_new(
            ModelDiscoverySource::Discovered {
                provider: "ollama".into()
            },
            vec![],
            vec![ModelDiscoveryFailure::named(
                "broken",
                "source_malformed",
                "invalid model facts",
            )],
            false,
        )
        .is_err()
    );
}

#[test]
fn supplied_system_user_and_discovered_rows_use_price_free_metering() {
    let supplied = row(
        "openai/gpt-5.6",
        ModelFactProvenance::supplied(SuppliedAuthority::System),
        MeteringKind::Currency,
    );
    let discovered = row(
        "ollama/gemma4:12b",
        ModelFactProvenance::discovered("ollama"),
        MeteringKind::Unmetered,
    );
    assert!(matches!(supplied.metering, MeteringKind::Currency));
    assert!(matches!(discovered.metering, MeteringKind::Unmetered));
    let subscription = row(
        "github-copilot/gpt-5.6",
        ModelFactProvenance::supplied(SuppliedAuthority::User),
        MeteringKind::Subscription,
    );
    assert!(matches!(subscription.metering, MeteringKind::Subscription));
}

#[test]
fn cache_codec_is_v2_and_round_trips_discovery_entries_only() {
    let cache = DiscoveryCacheV2::default();
    let bytes = cache.encode().unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["v"],
        2
    );
    let decoded = DiscoveryCacheV2::decode(&bytes).unwrap();
    assert!(decoded.providers.is_empty());
    assert!(matches!(
        DiscoveryCacheV2::decode(br#"{"v":1,"providers":{}}"#),
        Err(ego_ai::catalogue::DiscoveryCacheDecodeError::ObsoleteSchema { found: 1, .. })
    ));
    assert!(DiscoveryCacheV2::decode(br#"{"v":99,"providers":{}}"#).is_err());
    assert!(DiscoveryCacheV2::decode(b"not-json").is_err());
}

#[test]
fn complete_merge_replaces_rows_clears_failures_and_removes_absent_models() {
    let baseline = slice(&["ollama/old", "ollama/removed"], None);
    let report = ModelDiscoveryReport::try_new(
        ModelDiscoverySource::Discovered {
            provider: "ollama".into(),
        },
        vec![discovered_entry("ollama/new")],
        Vec::new(),
        true,
    )
    .unwrap();
    let merged = baseline.merge(report);
    assert_eq!(
        merged
            .models
            .iter()
            .map(|m| m.entry.slug.to_string())
            .collect::<Vec<_>>(),
        ["ollama/new"]
    );
    assert!(merged.models.iter().all(|m| m.failure.is_none()));
    assert!(merged.source_failure.is_none());
}

#[test]
fn partial_merge_keeps_named_failed_rows_and_marks_unobserved_rows_with_source_failure() {
    let baseline = slice(&["ollama/good", "ollama/broken", "ollama/unobserved"], None);
    let report = ModelDiscoveryReport::try_new(
        ModelDiscoverySource::Discovered {
            provider: "ollama".into(),
        },
        vec![discovered_entry("ollama/good")],
        vec![
            ModelDiscoveryFailure::named("broken", "source_malformed", "bad facts"),
            ModelDiscoveryFailure::source("source_unreachable", "daemon unavailable"),
        ],
        false,
    )
    .unwrap();
    let merged = baseline.merge(report);
    assert!(
        merged
            .models
            .iter()
            .any(|m| m.entry.slug.to_string() == "ollama/good" && m.failure.is_none())
    );
    assert!(
        merged
            .models
            .iter()
            .any(|m| m.entry.slug.to_string() == "ollama/broken" && m.failure.is_some())
    );
    assert!(
        merged
            .models
            .iter()
            .any(|m| m.entry.slug.to_string() == "ollama/unobserved"
                && m.failure.as_ref().unwrap().model.is_none())
    );
    assert!(merged.source_failure.is_some());
}

#[test]
fn total_failure_retains_last_known_rows_and_carries_the_unnamed_failure_per_row() {
    let baseline = slice(&["ollama/gemma4:12b"], None);
    let failure = ModelDiscoveryFailure::source("source_unreachable", "connection refused");
    let report = ModelDiscoveryReport::try_new(
        ModelDiscoverySource::Discovered {
            provider: "ollama".into(),
        },
        Vec::new(),
        vec![failure.clone()],
        false,
    )
    .unwrap();
    let merged = baseline.merge(report);
    assert_eq!(merged.models.len(), 1);
    assert_eq!(
        merged.models[0].failure.as_ref().unwrap().cause,
        failure.cause
    );
    assert_eq!(merged.source_failure.as_ref().unwrap().cause, failure.cause);
}

#[test]
fn complete_empty_discovery_is_an_authoritative_cached_empty_slice() {
    let mut cache = DiscoveryCacheV2::default();
    let report = ModelDiscoveryReport::try_new(
        ModelDiscoverySource::Discovered {
            provider: "ollama".into(),
        },
        Vec::new(),
        Vec::new(),
        true,
    )
    .unwrap();
    cache.merge_provider("ollama", report).unwrap();
    let slice = cache.providers.get("ollama").unwrap();
    assert!(slice.models.is_empty());
    assert!(slice.source_failure.is_none());
}

#[test]
fn cache_rejects_wrong_discovered_provider_and_supplied_source() {
    let mut cache = DiscoveryCacheV2::default();
    let wrong_provider = ModelDiscoveryReport::try_new(
        ModelDiscoverySource::Discovered {
            provider: "openai".into(),
        },
        Vec::new(),
        Vec::new(),
        true,
    )
    .unwrap();
    assert!(cache.merge_provider("ollama", wrong_provider).is_err());
    assert!(!cache.providers.contains_key("ollama"));

    let supplied =
        ModelDiscoveryReport::try_new(ModelDiscoverySource::Supplied, Vec::new(), Vec::new(), true)
            .unwrap();
    assert!(cache.merge_provider("ollama", supplied).is_err());
    assert!(!cache.providers.contains_key("ollama"));
}

#[test]
fn resolver_selected_supplied_enumeration_uses_facts_without_transport() {
    let (http, stub) = unasked();
    let catalogue = ego_ai::catalogue::Catalogue::new([ego_ai::provider::ProviderRow::openai()])
        .with_hosted(
            "openai/gpt-5.6",
            ego_ai::catalogue::HostedModel {
                capabilities: capabilities(),
                authority: SuppliedAuthority::System,
                metering: ego_ai::catalogue::tabulated(),
                rates: None,
            },
        );
    let row = catalogue.row("openai").unwrap();
    let report = block_on(row.resolver.enumerate(&ego_ai::resolver::Enumeration {
        catalogue: &catalogue,
        row,
        http: &http,
        credential: None,
    }))
    .unwrap();
    assert_eq!(report.models().len(), 1);
    assert!(matches!(
        report.models()[0].provenance,
        ModelFactProvenance::Supplied {
            authority: SuppliedAuthority::System
        }
    ));
    assert!(stub.paths().is_empty());
}

#[test]
fn resolver_selected_subscription_enumeration_projects_subscription_metering() {
    let (http, stub) = unasked();
    let catalogue =
        ego_ai::catalogue::Catalogue::new(
            [ego_ai::provider::ProviderRow::github_copilot_default()],
        )
        .with_hosted(
            "github-copilot/gpt-5.6",
            ego_ai::catalogue::HostedModel {
                capabilities: capabilities(),
                authority: SuppliedAuthority::User,
                metering: ego_ai::catalogue::subscription("subscription-v1"),
                rates: None,
            },
        );
    let row = catalogue.row("github-copilot").unwrap();
    let report = block_on(row.resolver.enumerate(&ego_ai::resolver::Enumeration {
        catalogue: &catalogue,
        row,
        http: &http,
        credential: None,
    }))
    .unwrap();
    assert_eq!(report.models().len(), 1);
    assert!(matches!(
        report.models()[0].provenance,
        ModelFactProvenance::Supplied {
            authority: SuppliedAuthority::User
        }
    ));
    assert!(matches!(
        report.models()[0].metering,
        MeteringKind::Subscription
    ));
    assert!(stub.paths().is_empty());
}

#[test]
fn resolver_selected_ollama_enumeration_uses_tags_only_and_leaves_ceilings_unknown() {
    let (http, stub) = server([(
        200,
        r#"{"models":[
        {"name":"gemma4:12b-mlx","capabilities":["completion","tools"]}
    ]}"#,
    )]);
    let catalogue =
        ego_ai::catalogue::Catalogue::new([ego_ai::provider::ProviderRow::ollama_default()]);
    let row = catalogue.row("ollama").unwrap();
    let report = block_on(row.resolver.enumerate(&ego_ai::resolver::Enumeration {
        catalogue: &catalogue,
        row,
        http: &http,
        credential: None,
    }))
    .unwrap();
    assert_eq!(report.models().len(), 1);
    assert_eq!(report.models()[0].slug.to_string(), "ollama/gemma4:12b-mlx");
    assert_eq!(report.models()[0].capabilities.max_context_tokens, None);
    assert_eq!(report.models()[0].capabilities.max_output_tokens, None);
    assert_eq!(stub.paths(), ["/api/tags"]);
}

fn discovered_entry(slug: &str) -> ModelDiscoveryEntry {
    ModelDiscoveryEntry {
        slug: ModelSlug::parse(slug).unwrap(),
        capabilities: capabilities(),
        metering: MeteringKind::Unmetered,
        provenance: ModelFactProvenance::discovered("ollama"),
    }
}

fn slice(slugs: &[&str], source_failure: Option<ModelDiscoveryFailure>) -> CachedDiscoverySlice {
    CachedDiscoverySlice {
        models: slugs
            .iter()
            .map(|slug| CachedDiscoveryModel {
                entry: discovered_entry(slug),
                failure: None,
            })
            .collect(),
        source_failure,
    }
}
