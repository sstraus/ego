//! The process boundary for the shared model catalogue snapshot.

use std::collections::{BTreeMap, BTreeSet};

use ego_ai::catalogue::{
    CachedDiscoverySlice, DiscoveryCacheDecodeError, DiscoveryCacheV2, FactFreshness,
    ModelAvailability, ModelAvailabilityCause, ModelCatalogueEntry, ModelCatalogueSnapshot,
    ModelCatalogueSourceStatus, ModelDiscoveryFailure, ModelDiscoveryFailureCause,
    ModelDiscoverySource, ModelFactProvenance,
};
use ego_ai::provider::{EnvCredentials, ProviderRow};
use ego_ai::resolver::{Enumeration, ResolverCredentialRequirement};
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
use crate::onboarding::{EnvironmentPresence, credential_present, discover_ollama};
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
    let (mut cache, _) = read_cache(&cache_store, args.refresh)?;
    let needs_observation = selected
        .iter()
        .any(|row| args.refresh || !cache.providers.contains_key(row.id()));

    let mut supplied_entries = Vec::new();
    let mut supplied_sources = unsupported_sources;
    let mut current_discovered = BTreeSet::new();
    let mut current_discovered_sources = BTreeSet::new();
    let mut current_source_failures = BTreeMap::new();
    let mut missing_enumeration_credentials = BTreeMap::new();
    let mut resolved_enumeration_credentials = BTreeSet::new();

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
        let (latest, obsolete) = read_cache(&cache_store, args.refresh)?;
        cache = latest;
        let mut publish = obsolete;

        for row in &selected {
            let observe = args.refresh || !cache.providers.contains_key(row.id());
            if !observe {
                continue;
            }
            let credential = match row.resolver.enumeration_credential() {
                ResolverCredentialRequirement::None => None,
                ResolverCredentialRequirement::Required => {
                    let credentials = crate::auth::request_credentials(
                        row.id(),
                        auth.get(row.id())
                            .and_then(|provider| provider.credential_command.as_deref()),
                        http.as_ref(),
                    )
                    .await
                    .map_err(|error| RunTermination::Provider {
                        message: error.to_string(),
                    })?;
                    match row.credential.resolve(credentials.as_ref()) {
                        Ok(credential) => {
                            resolved_enumeration_credentials.insert(row.id().to_owned());
                            Some(credential)
                        }
                        Err(error) => {
                            if obsolete && args.provider.as_deref() == Some(row.id()) {
                                return Err(RunTermination::Provider {
                                    message: error.to_string(),
                                });
                            }
                            missing_enumeration_credentials
                                .insert(row.id().to_owned(), error.to_string());
                            continue;
                        }
                    }
                }
            };
            let report = row
                .resolver
                .enumerate(&Enumeration {
                    catalogue: &catalogue,
                    row,
                    http,
                    credential: credential.as_ref(),
                })
                .await
                .map_err(|message| RunTermination::Failed { message })?;

            if obsolete && (!report.is_complete() || !report.failures().is_empty()) {
                let failures = report
                    .failures()
                    .iter()
                    .map(|failure| match &failure.model {
                        Some(model) => format!("model `{model}`: {}", failure.message),
                        None => failure.message.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(RunTermination::Provider {
                    message: format!(
                        "provider `{}` could not rebuild the obsolete model cache: {failures}",
                        row.id()
                    ),
                });
            }

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
        let missing_credential = missing_enumeration_credentials
            .get(row.id())
            .map(|message| missing_credential(message.clone()))
            .or_else(|| {
                (row.resolver.enumeration_credential() == ResolverCredentialRequirement::Required
                    && !resolved_enumeration_credentials.contains(row.id()))
                .then(|| credential_for(row))
                .filter(|availability| {
                    matches!(
                        availability,
                        ModelAvailability::Unavailable {
                            cause: ModelAvailabilityCause::MissingCredential,
                            ..
                        }
                    )
                })
            });
        let Some(slice) = cache.providers.get(row.id()) else {
            if let Some(availability) = missing_credential {
                sources.push(ModelCatalogueSourceStatus {
                    provider: row.id().to_owned(),
                    provenance: ModelFactProvenance::discovered(row.id().to_owned()),
                    freshness: FactFreshness::Current,
                    availability,
                });
            }
            continue;
        };
        project_cached(
            row,
            slice,
            &current_discovered,
            &current_discovered_sources,
            current_source_failures.get(row.id()),
            missing_credential.as_ref(),
            &mut models,
            &mut sources,
        );
        // Only OpenRouter resolves user `models.toml` rows at run time, so only its
        // listing may show them; elsewhere they would list facts the run ignores.
        if row.id() == "openrouter" {
            let supplied = catalogue
                .enumerate_hosted(row.id(), None)
                .map_err(|message| RunTermination::Failed { message })?;
            overlay_supplied(supplied.models(), missing_credential.as_ref(), &mut models);
        }
    }

    Ok(ModelCatalogueSnapshot::from_entries_and_sources(
        models, sources,
    ))
}

/// Project existing facts without resolving secrets or publishing a cache.
/// Only Ollama may perform discovery during first-run observation.
pub(crate) async fn build_startup_catalogue_snapshot(
    http: &std::sync::Arc<dyn ego_ai::http::Http>,
    environment: &dyn EnvironmentPresence,
    observed: &BTreeSet<String>,
) -> Result<
    (
        ModelCatalogueSnapshot,
        BTreeMap<String, ego_ai::catalogue::ModelSlug>,
    ),
    RunTermination,
> {
    let auth = config::auth().map_err(usage)?;
    let rows = model::rows(&auth).map_err(|error| usage_message(error.to_string()))?;
    let admitted = rows
        .iter()
        .map(|row| row.id().to_owned())
        .collect::<BTreeSet<_>>();
    let (hosted, defaults) = facts::supplied_with_defaults(&Ceilings::default()).map_err(usage)?;
    let supplied_providers = hosted
        .iter()
        .filter_map(|(slug, _)| {
            slug.split_once('/')
                .map(|(provider, _)| provider.to_owned())
        })
        .collect::<BTreeSet<_>>();
    let mut sources = unsupported_supplied_sources(&hosted, &admitted, None);
    let mut catalogue = ego_ai::catalogue::Catalogue::new(rows.iter().cloned());
    for (slug, facts) in hosted {
        catalogue = catalogue.with_hosted(&slug, facts);
    }
    let unavailable = |row: &ProviderRow| {
        mapping_unavailability(row).or_else(|| {
            (!credential_present(&row.credential, environment, observed)).then(|| {
                missing_credential(format!("no credential is configured for provider `{}`; run `ego auth login {}` for setup instructions", row.id(), row.id()))
            })
        })
    };
    let mut models = Vec::new();
    for row in &rows {
        // Ollama's resolver is the one credential-free network enumerator;
        // it is observed only through the bounded boundary below.
        if row.id() == ego_ai::catalogue::OLLAMA
            || !supplied_providers.contains(row.id())
            || row.resolver.enumeration_credential() != ResolverCredentialRequirement::None
        {
            continue;
        }
        let report = row
            .resolver
            .enumerate(&Enumeration {
                catalogue: &catalogue,
                row,
                http,
                credential: None,
            })
            .await
            .map_err(|message| RunTermination::Failed { message })?;
        if report.source() == &ModelDiscoverySource::Supplied {
            project_supplied_with_availability(
                row,
                report.models(),
                unavailable(row).unwrap_or(ModelAvailability::Available),
                &mut models,
                &mut sources,
            );
        }
    }
    let mut cache = match config::ego_home() {
        Some(home) => read_cache(&ModelCatalogueCache::new(&OsIo, &home), false)?.0,
        None => DiscoveryCacheV2::default(),
    };
    let mut current = BTreeSet::new();
    let mut current_sources = BTreeSet::new();
    let mut source_failures = BTreeMap::new();
    if let Ok(row) = catalogue.row(ego_ai::catalogue::OLLAMA) {
        let report = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            discover_ollama(row, http, environment.is_set(model::OLLAMA_HOST)),
        )
        .await
        .unwrap_or_else(|_| {
            Err("startup Ollama discovery exceeded its two-second deadline".to_owned())
        });
        let report = match report {
            Ok(report) => report,
            Err(message) => ego_ai::catalogue::ModelDiscoveryReport::try_new(
                ModelDiscoverySource::Discovered {
                    provider: row.id().to_owned(),
                },
                Vec::new(),
                vec![ModelDiscoveryFailure::source("source_unreachable", message)],
                false,
            )
            .map_err(|message| RunTermination::Failed { message })?,
        };
        current.extend(report.models().iter().map(|entry| entry.slug.clone()));
        current_sources.insert(row.id().to_owned());
        if let Some(failure) = report
            .failures()
            .iter()
            .find(|failure| failure.model.is_none())
        {
            source_failures.insert(row.id().to_owned(), failure.clone());
        }
        cache
            .merge_provider(row.id(), report)
            .map_err(|message| RunTermination::Failed { message })?;
    }
    for row in &rows {
        let observed_unavailability = unavailable(row);
        if let Some(slice) = cache.providers.get(row.id()) {
            project_cached(
                row,
                slice,
                &current,
                &current_sources,
                source_failures.get(row.id()),
                observed_unavailability.as_ref(),
                &mut models,
                &mut sources,
            );
        } else if row.resolver.enumeration_credential() == ResolverCredentialRequirement::Required {
            sources.push(ModelCatalogueSourceStatus {
                provider: row.id().to_owned(),
                provenance: ModelFactProvenance::discovered(row.id().to_owned()),
                freshness: FactFreshness::Current,
                availability: observed_unavailability.unwrap_or_else(|| ModelAvailability::Unavailable {
                    cause: ModelAvailabilityCause::IncompleteFacts,
                    message: format!("provider `{}` has no observed model inventory; run `ego models --provider {} --refresh` to populate it", row.id(), row.id()),
                }),
            });
        }
    }
    Ok((
        ModelCatalogueSnapshot::from_entries_and_sources(models, sources),
        defaults,
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
        // The strongest layer that supplied any row for this provider.
        // `SuppliedAuthority` orders `Builtin < System < User`, so `max` is
        // the same rule the merge in `facts` applies, said once.
        authorities
            .entry(provider.to_string())
            .and_modify(|authority| *authority = (*authority).max(facts.authority))
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

fn read_cache(
    cache: &ModelCatalogueCache<'_>,
    refresh: bool,
) -> Result<(DiscoveryCacheV2, bool), RunTermination> {
    match cache.read_bytes().map_err(internal)? {
        Some(bytes) => match DiscoveryCacheV2::decode(&bytes) {
            Ok(cache) => Ok((cache, false)),
            Err(DiscoveryCacheDecodeError::ObsoleteSchema { .. }) if refresh => {
                Ok((DiscoveryCacheV2::default(), true))
            }
            Err(error @ DiscoveryCacheDecodeError::ObsoleteSchema { .. }) => {
                Err(RunTermination::UsageError {
                    message: format!(
                        "{error}; run `ego models --refresh` to rebuild the current cache"
                    ),
                })
            }
            Err(error @ DiscoveryCacheDecodeError::UnsupportedSchema { .. })
            | Err(error @ DiscoveryCacheDecodeError::Corrupt(_)) => Err(RunTermination::Failed {
                message: error.to_string(),
            }),
        },
        None => Ok((DiscoveryCacheV2::default(), false)),
    }
}

fn mapping_unavailability(row: &ProviderRow) -> Option<ModelAvailability> {
    ego_ai::catalogue::live_promotion_refusal(row).map(|refusal| ModelAvailability::Unavailable {
        cause: ModelAvailabilityCause::MappingUnavailable,
        message: refusal.to_string(),
    })
}

fn project_supplied(
    row: &ProviderRow,
    entries: &[ego_ai::catalogue::ModelDiscoveryEntry],
    models: &mut Vec<ModelCatalogueEntry>,
    sources: &mut Vec<ModelCatalogueSourceStatus>,
) {
    // Promotion first, credential second. A row waiting on live evidence
    // cannot be run with any key, so reporting `missing_credential` for it
    // would name a fix that does not work — and reading the credential to
    // find that out would run an operator's `credential_command` for a row
    // nothing may select.
    let credential = mapping_unavailability(row).unwrap_or_else(|| credential_for(row));
    project_supplied_with_availability(row, entries, credential, models, sources);
}

fn project_supplied_with_availability(
    row: &ProviderRow,
    entries: &[ego_ai::catalogue::ModelDiscoveryEntry],
    credential: ModelAvailability,
    models: &mut Vec<ModelCatalogueEntry>,
    sources: &mut Vec<ModelCatalogueSourceStatus>,
) {
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

fn missing_credential(message: String) -> ModelAvailability {
    ModelAvailability::Unavailable {
        cause: ModelAvailabilityCause::MissingCredential,
        message,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "cached projection keeps independent freshness, failure, credential, and output channels explicit"
)]
fn project_cached(
    row: &ProviderRow,
    slice: &CachedDiscoverySlice,
    current: &BTreeSet<ego_ai::catalogue::ModelSlug>,
    current_sources: &BTreeSet<String>,
    current_source_failure: Option<&ModelDiscoveryFailure>,
    credential_unavailability: Option<&ModelAvailability>,
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
            availability: credential_unavailability.cloned().unwrap_or_else(|| {
                cached
                    .failure
                    .as_ref()
                    .map_or(ModelAvailability::Available, discovery_unavailable)
            }),
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
        availability: credential_unavailability
            .cloned()
            .unwrap_or_else(|| failure.map_or(ModelAvailability::Available, discovery_unavailable)),
    });
}

/// Operator-supplied rows stand over what a provider's listing states: they
/// replace a listed model's facts and add a model the listing lacks.
fn overlay_supplied(
    supplied: &[ego_ai::catalogue::ModelDiscoveryEntry],
    credential_unavailability: Option<&ModelAvailability>,
    models: &mut Vec<ModelCatalogueEntry>,
) {
    for entry in supplied {
        let overlaid = ModelCatalogueEntry {
            slug: entry.slug.clone(),
            capabilities: entry.capabilities.clone(),
            metering: entry.metering,
            provenance: entry.provenance.clone(),
            freshness: FactFreshness::Current,
            availability: credential_unavailability
                .cloned()
                .unwrap_or(ModelAvailability::Available),
        };
        match models.iter_mut().find(|listed| listed.slug == entry.slug) {
            Some(listed) => *listed = overlaid,
            None => models.push(overlaid),
        }
    }
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
            // Named rather than folded into "system": the whole reason a
            // reader looks at this column is to find out whether a row came
            // from a file they can edit, and "builtin" is the answer that
            // tells them it did not.
            ego_ai::catalogue::SuppliedAuthority::Builtin => "builtin".to_string(),
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
