//! The process boundary for the shared model catalogue snapshot.

use std::collections::{BTreeMap, BTreeSet};

use ego_ai::catalogue::{
    CachedDiscoverySlice, DiscoveryCacheV1, FactFreshness, ModelAvailability,
    ModelAvailabilityCause, ModelCatalogueEntry, ModelCatalogueSnapshot,
    ModelCatalogueSourceStatus, ModelDiscoveryFailure, ModelDiscoveryFailureCause,
    ModelDiscoverySource, ModelFactProvenance,
};
use ego_ai::provider::{EnvCredentials, ProviderRow};
use ego_ai::resolver::Enumeration;
use ego_core::ceiling::Ceilings;
use ego_core::event::time::Clock;
use ego_core::generator::capabilities::ReasoningEffortSupport;
use ego_core::run::RunTermination;
use ego_core::turn::CancelToken;
use ego_tools::clock::SystemClock;
use ego_tools::model_catalogue_cache::ModelCatalogueCache;
use ego_tools::store::OsIo;

use crate::auth::StoreCredentials;
use crate::cli::ModelsArgs;
use crate::{config, facts, model};

/// Build and render the one snapshot consumed by every model-listing surface.
pub async fn execute(args: &ModelsArgs, cancel: &CancelToken) -> Result<String, RunTermination> {
    let scope = crate::provider_http::CommandHttpScope::online(cancel.clone())
        .await
        .map_err(|message| RunTermination::Provider { message })?;
    let snapshot = build_catalogue_snapshot(args, scope.http()).await?;
    if args.json {
        serde_json::to_string(&snapshot).map_err(|error| RunTermination::Failed {
            message: format!("the model catalogue could not be rendered: {error}"),
        })
    } else {
        Ok(render_text(&snapshot).join("\n"))
    }
}

/// Resolve the shared catalogue authority consumed by every interactive and
/// non-interactive model surface.
pub async fn build_catalogue_snapshot(
    args: &ModelsArgs,
    http: &std::sync::Arc<dyn ego_ai::http::Http>,
) -> Result<ModelCatalogueSnapshot, RunTermination> {
    let auth = config::auth().map_err(usage)?;
    let rows = model::rows(&auth).map_err(|error| usage_message(error.to_string()))?;
    let admitted = rows
        .iter()
        .map(|row| row.id().to_owned())
        .collect::<BTreeSet<_>>();
    let selected = select_rows(rows, args.provider.as_deref())?;

    // Filtering is settled before credentials, cache, or model facts.
    let hosted = facts::supplied(&Ceilings::default()).map_err(usage)?;
    let unsupported_sources =
        unsupported_supplied_sources(&hosted, &admitted, args.provider.as_deref());
    let mut catalogue = ego_ai::catalogue::Catalogue::new(selected.iter().cloned());
    for (slug, facts) in hosted {
        catalogue = catalogue.with_hosted(&slug, facts);
    }

    let home = config::ego_home().ok_or_else(|| RunTermination::Failed {
        message: "the model catalogue needs EGO_HOME or HOME for its durable cache".to_string(),
    })?;
    let io = OsIo;
    let cache_store = ModelCatalogueCache::new(&io, &home);
    let mut cache = read_cache(&cache_store)?;
    let needs_observation = selected
        .iter()
        .any(|row| args.refresh || !cache.providers.contains_key(row.id()));

    let mut supplied_entries = Vec::new();
    let mut supplied_sources = unsupported_sources;
    let mut current_discovered = BTreeSet::new();
    let mut current_discovered_sources = BTreeSet::new();
    let mut current_source_failures = BTreeMap::new();

    if needs_observation {
        let lease = cache_store
            .lease(
                SystemClock.now(),
                std::process::id(),
                &crate::session::assume_alive,
            )
            .map_err(internal)?;
        // The lease owner must merge against the newest baseline, not the one
        // read before it waited for another process.
        cache = read_cache(&cache_store)?;
        let mut publish = false;

        for row in &selected {
            let observe = args.refresh || !cache.providers.contains_key(row.id());
            if !observe {
                continue;
            }
            let report = row
                .resolver
                .enumerate(&Enumeration {
                    catalogue: &catalogue,
                    row,
                    http,
                })
                .await
                .map_err(|message| RunTermination::Failed { message })?;

            match report.source().clone() {
                ModelDiscoverySource::Supplied => {
                    project_supplied(
                        row,
                        report.models(),
                        &mut supplied_entries,
                        &mut supplied_sources,
                    );
                }
                ModelDiscoverySource::Discovered { provider } => {
                    let observed = report
                        .models()
                        .iter()
                        .map(|entry| entry.slug.clone())
                        .collect::<Vec<_>>();
                    let source_failure = report
                        .failures()
                        .iter()
                        .find(|failure| failure.model.is_none())
                        .cloned();
                    cache
                        .merge_provider(row.id(), report)
                        .map_err(|message| RunTermination::Failed { message })?;
                    current_discovered.extend(observed);
                    current_discovered_sources.insert(provider.clone());
                    if let Some(failure) = source_failure {
                        current_source_failures.insert(provider, failure);
                    }
                    publish = true;
                }
            }
        }

        if publish {
            let bytes = cache
                .encode()
                .map_err(|message| RunTermination::Failed { message })?;
            cache_store
                .publish_under_lease(&lease, &bytes)
                .map_err(internal)?;
        }
    }

    let mut models = supplied_entries;
    let mut sources = supplied_sources;
    for row in &selected {
        let Some(slice) = cache.providers.get(row.id()) else {
            continue;
        };
        project_cached(
            row,
            slice,
            &current_discovered,
            &current_discovered_sources,
            current_source_failures.get(row.id()),
            &mut models,
            &mut sources,
        );
    }

    Ok(ModelCatalogueSnapshot::from_entries_and_sources(
        models, sources,
    ))
}

fn unsupported_supplied_sources(
    hosted: &[(String, facts::Hosted)],
    admitted: &BTreeSet<String>,
    selected: Option<&str>,
) -> Vec<ModelCatalogueSourceStatus> {
    let mut authorities: BTreeMap<String, ego_ai::catalogue::SuppliedAuthority> = BTreeMap::new();
    for (slug, facts) in hosted {
        let Some((provider, _)) = slug.split_once('/') else {
            continue;
        };
        if selected.is_some_and(|selected| selected != provider) {
            continue;
        }
        if admitted.contains(provider) {
            continue;
        }
        authorities
            .entry(provider.to_string())
            .and_modify(|authority| {
                if facts.authority == ego_ai::catalogue::SuppliedAuthority::User {
                    *authority = facts.authority;
                }
            })
            .or_insert(facts.authority);
    }
    authorities
        .into_iter()
        .map(|(provider, authority)| {
            ModelCatalogueSourceStatus::unavailable(
                provider.clone(),
                ModelFactProvenance::supplied(authority),
                FactFreshness::Current,
                "mapping_unavailable",
                format!("provider `{provider}` has no current selectable mapping"),
            )
        })
        .collect()
}

fn select_rows(
    rows: Vec<ProviderRow>,
    provider: Option<&str>,
) -> Result<Vec<ProviderRow>, RunTermination> {
    let Some(provider) = provider else {
        return Ok(rows);
    };
    rows.into_iter()
        .find(|row| row.id() == provider)
        .map(|row| vec![row])
        .ok_or_else(|| usage_message(format!("no admitted model provider `{provider}`")))
}

fn read_cache(cache: &ModelCatalogueCache<'_>) -> Result<DiscoveryCacheV1, RunTermination> {
    match cache.read_bytes().map_err(internal)? {
        Some(bytes) => {
            DiscoveryCacheV1::decode(&bytes).map_err(|message| RunTermination::Failed { message })
        }
        None => Ok(DiscoveryCacheV1::default()),
    }
}

fn project_supplied(
    row: &ProviderRow,
    entries: &[ego_ai::catalogue::ModelDiscoveryEntry],
    models: &mut Vec<ModelCatalogueEntry>,
    sources: &mut Vec<ModelCatalogueSourceStatus>,
) {
    let credential = credential_for(row);
    for entry in entries {
        models.push(ModelCatalogueEntry {
            slug: entry.slug.clone(),
            capabilities: entry.capabilities.clone(),
            metering: entry.metering,
            provenance: entry.provenance.clone(),
            freshness: FactFreshness::Current,
            availability: credential.clone(),
        });
    }
    if let Some(provenance) = entries.first().map(|entry| entry.provenance.clone()) {
        sources.push(ModelCatalogueSourceStatus {
            provider: row.id().to_owned(),
            provenance,
            freshness: FactFreshness::Current,
            availability: credential,
        });
    }
}

fn credential_for(row: &ProviderRow) -> ModelAvailability {
    let resolved = match config::ego_home() {
        Some(home) => row.credential.resolve(&StoreCredentials::new(home)),
        None => row.credential.resolve(&EnvCredentials),
    };
    match resolved {
        Ok(_) => ModelAvailability::Available,
        Err(error) => ModelAvailability::Unavailable {
            cause: ModelAvailabilityCause::MissingCredential,
            message: error.to_string(),
        },
    }
}

fn project_cached(
    row: &ProviderRow,
    slice: &CachedDiscoverySlice,
    current: &BTreeSet<ego_ai::catalogue::ModelSlug>,
    current_sources: &BTreeSet<String>,
    current_source_failure: Option<&ModelDiscoveryFailure>,
    models: &mut Vec<ModelCatalogueEntry>,
    sources: &mut Vec<ModelCatalogueSourceStatus>,
) {
    for cached in &slice.models {
        models.push(ModelCatalogueEntry {
            slug: cached.entry.slug.clone(),
            capabilities: cached.entry.capabilities.clone(),
            metering: cached.entry.metering,
            provenance: cached.entry.provenance.clone(),
            freshness: if current.contains(&cached.entry.slug) {
                FactFreshness::Current
            } else {
                FactFreshness::Cached
            },
            availability: cached
                .failure
                .as_ref()
                .map_or(ModelAvailability::Available, discovery_unavailable),
        });
    }
    let failure = current_source_failure.or(slice.source_failure.as_ref());
    sources.push(ModelCatalogueSourceStatus {
        provider: row.id().to_owned(),
        provenance: ModelFactProvenance::discovered(row.id().to_owned()),
        freshness: if current_sources.contains(row.id()) {
            FactFreshness::Current
        } else {
            FactFreshness::Cached
        },
        availability: failure.map_or(ModelAvailability::Available, discovery_unavailable),
    });
}

fn discovery_unavailable(failure: &ModelDiscoveryFailure) -> ModelAvailability {
    let cause = match failure.cause {
        ModelDiscoveryFailureCause::SourceUnreachable => ModelAvailabilityCause::SourceUnreachable,
        ModelDiscoveryFailureCause::SourceMalformed => ModelAvailabilityCause::SourceMalformed,
        ModelDiscoveryFailureCause::IncompleteFacts => ModelAvailabilityCause::IncompleteFacts,
    };
    ModelAvailability::Unavailable {
        cause,
        message: failure.message.clone(),
    }
}

fn render_text(snapshot: &ModelCatalogueSnapshot) -> Vec<String> {
    let mut lines = crate::catalogue::plain_table(
        vec![vec![
            "SLUG".into(),
            "CONTEXT".into(),
            "OUTPUT".into(),
            "REASONING".into(),
            "METERING".into(),
            "SOURCE".into(),
            "FRESHNESS".into(),
            "AVAILABILITY".into(),
        ]]
        .into_iter()
        .chain(snapshot.models.iter().map(|entry| {
            vec![
                entry.slug.to_string(),
                ceiling(entry.capabilities.max_context_tokens),
                ceiling(entry.capabilities.max_output_tokens),
                reasoning(&entry.capabilities.reasoning_efforts),
                metering(entry.metering).into(),
                provenance(&entry.provenance),
                freshness(entry.freshness).into(),
                availability(&entry.availability),
            ]
        }))
        .collect(),
    );
    lines.push("SOURCES".to_string());
    lines.extend(crate::catalogue::plain_table(
        vec![vec![
            "PROVIDER".into(),
            "SOURCE".into(),
            "FRESHNESS".into(),
            "AVAILABILITY".into(),
        ]]
        .into_iter()
        .chain(snapshot.sources.iter().map(|source| {
            vec![
                source.provider.clone(),
                provenance(&source.provenance),
                freshness(source.freshness).into(),
                availability(&source.availability),
            ]
        }))
        .collect(),
    ));
    lines
}

fn ceiling(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
}

fn reasoning(support: &ReasoningEffortSupport) -> String {
    match support {
        ReasoningEffortSupport::Unknown => "unknown".to_string(),
        ReasoningEffortSupport::Any => "any".to_string(),
        ReasoningEffortSupport::Only(levels) => levels
            .iter()
            .map(|level| level.as_str())
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn metering(kind: ego_core::public::MeteringKind) -> &'static str {
    match kind {
        ego_core::public::MeteringKind::Currency => "currency",
        ego_core::public::MeteringKind::Subscription => "subscription",
        ego_core::public::MeteringKind::Unmetered => "unmetered",
    }
}

fn provenance(value: &ModelFactProvenance) -> String {
    match value {
        ModelFactProvenance::Supplied { authority } => match authority {
            ego_ai::catalogue::SuppliedAuthority::System => "system".to_string(),
            ego_ai::catalogue::SuppliedAuthority::User => "user".to_string(),
        },
        ModelFactProvenance::Discovered { provider } => format!("provider:{provider}"),
    }
}

fn freshness(value: FactFreshness) -> &'static str {
    match value {
        FactFreshness::Current => "current",
        FactFreshness::Cached => "cached",
    }
}

fn availability(value: &ModelAvailability) -> String {
    match value {
        ModelAvailability::Available => "available".to_string(),
        ModelAvailability::Unavailable { cause, message } => {
            format!("{}: {message}", availability_cause(*cause))
        }
    }
}

fn availability_cause(cause: ModelAvailabilityCause) -> &'static str {
    match cause {
        ModelAvailabilityCause::MissingCredential => "missing_credential",
        ModelAvailabilityCause::SourceUnreachable => "source_unreachable",
        ModelAvailabilityCause::SourceMalformed => "source_malformed",
        ModelAvailabilityCause::IncompleteFacts => "incomplete_facts",
        ModelAvailabilityCause::MappingUnavailable => "mapping_unavailable",
    }
}

fn usage(error: crate::cli::UsageError) -> RunTermination {
    usage_message(error.to_string())
}

fn usage_message(message: String) -> RunTermination {
    RunTermination::UsageError { message }
}

fn internal(error: impl std::fmt::Display) -> RunTermination {
    RunTermination::Failed {
        message: error.to_string(),
    }
}
