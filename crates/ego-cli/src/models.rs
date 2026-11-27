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

use crate::cli::ModelsArgs;
use crate::onboarding::{EnvironmentPresence, credential_present, discover_ollama};
use crate::{config, facts, model};

type DiscoveryConnection = (String, ego_core::config::ConnectionConfig);

/// Resolve stable identity before any row, credential or account cache is used.
fn discovery_connection(
    connections: &BTreeMap<String, ego_core::config::ConnectionConfig>,
    selected: Option<&str>,
    provider: Option<&str>,
) -> Result<Option<DiscoveryConnection>, String> {
    let connection = ego_core::config::select_connection(
        connections,
        selected,
        provider.map(crate::auth::connection_vendor),
    )?;
    if let Some((id, connection)) = connection {
        connection.validate(id)?;
        let route = crate::auth::connection_route(connection);
        if let Some(provider) = provider
            && provider != route
        {
            return Err(format!(
                "selected account uses route {route}, not {provider}"
            ));
        }
        Ok(Some((id.to_owned(), connection.clone())))
    } else {
        Ok(None)
    }
}

/// A host's configured model may select an instance, never an account route.
pub(crate) fn selected_instance_provider(effective: &ego_core::config::Effective) -> Option<&str> {
    effective
        .model
        .as_ref()?
        .0
        .split_once('/')
        .map(|(provider, _)| provider)
        .filter(|provider| {
            effective
                .providers
                .instances
                .keys()
                .any(|id| id.as_str() == *provider)
        })
}

/// Explicit instances own their credential binding; ambient account selection cannot redirect them.
pub(crate) fn instance_discovery_connection(
    providers: &ego_core::config::ResolvedProviders,
    connections: &BTreeMap<String, ego_core::config::ConnectionConfig>,
    selected: Option<&str>,
    provider: Option<&str>,
) -> Result<Option<DiscoveryConnection>, String> {
    if let Some(instance) = provider.and_then(|provider| {
        providers
            .instances
            .iter()
            .find(|(id, _)| id.as_str() == provider)
            .map(|(_, sourced)| &sourced.value)
    }) {
        if selected.is_some_and(|selected| {
            instance.credential_ref.as_ref().map(|id| id.as_str()) != Some(selected)
        }) {
            return Err("connection selection cannot rebind a configured provider instance".into());
        }
        return Ok(None);
    }
    if selected.is_none() && provider.is_none() && !providers.instances.is_empty() {
        return Ok(None);
    }
    discovery_connection(connections, selected, provider)
}

/// Instance authority is per row, not the ambient selected account. Rows and
/// credential references have already passed configured_catalogue_rows validation.
pub(crate) fn row_connection(
    row: &ProviderRow,
    selected: Option<&DiscoveryConnection>,
    providers: &ego_core::config::ResolvedProviders,
    connections: &BTreeMap<String, ego_core::config::ConnectionConfig>,
) -> Option<DiscoveryConnection> {
    if let Some(instance) = providers
        .instances
        .iter()
        .find(|(id, _)| id.as_str() == row.id())
        .map(|(_, value)| &value.value)
    {
        return instance.credential_ref.as_ref().and_then(|id| {
            connections
                .get(id.as_str())
                .map(|connection| (id.as_str().to_owned(), connection.clone()))
        });
    }
    selected.cloned()
}

pub(crate) fn configured_catalogue_rows(
    auth: &std::collections::HashMap<String, ego_core::config::AuthProviderLayer>,
    providers: &ego_core::config::ResolvedProviders,
    connections: &BTreeMap<String, ego_core::config::ConnectionConfig>,
) -> Result<(ego_ai::catalogue::Catalogue, Vec<ProviderRow>), String> {
    let mut rows = model::rows(auth).map_err(|error| error.to_string())?;
    let catalogue = ego_ai::catalogue::Catalogue::new(rows.iter().cloned())
        .with_instances(providers, connections)?;
    for id in providers.instances.keys() {
        rows.push(
            catalogue
                .row(id.as_str())
                .map_err(|error| error.to_string())?
                .clone(),
        );
    }
    Ok((catalogue, rows))
}

fn connection_cache<'a>(
    io: &'a dyn ego_tools::store::StoreIo,
    home: &std::path::Path,
    connection: Option<&str>,
) -> ModelCatalogueCache<'a> {
    let root = connection.map_or_else(
        || home.to_path_buf(),
        |id| home.join("connections").join(id),
    );
    ModelCatalogueCache::new(io, &root)
}

fn named_availability(
    id: &str,
    connection: &ego_core::config::ConnectionConfig,
    environment: &dyn EnvironmentPresence,
) -> ModelAvailability {
    let store = config::ego_home().map(crate::auth::AuthStore::new);
    if named_credential_present(id, connection, store.as_ref(), environment) {
        ModelAvailability::Available
    } else {
        missing_credential("selected account has no credential in its declared source".to_owned())
    }
}

/// Observe only the credential kind and source bound to this connection.
pub(crate) fn named_credential_present(
    id: &str,
    connection: &ego_core::config::ConnectionConfig,
    store: Option<&crate::auth::AuthStore>,
    environment: &dyn EnvironmentPresence,
) -> bool {
    match connection.access {
        ego_core::config::ConnectionAccess::Key => {
            crate::auth::key_connection_present(id, connection, store, environment)
        }
        ego_core::config::ConnectionAccess::Oauth => store
            .and_then(|store| store.read_observed().ok())
            .is_some_and(|entries| {
                entries
                    .get(&crate::auth::connection_storage_key(id))
                    .is_some_and(crate::auth::AuthEntry::has_oauth)
            }),
    }
}

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
    let mut effective =
        config::user_effective_with_connection(args.connection.as_deref()).map_err(usage)?;
    if args.connection.is_none()
        && let Some(route) = args.provider.as_deref()
        && let Some(connection) = effective
            .connection
            .as_ref()
            .and_then(|id| effective.connections.get(id))
        && crate::auth::connection_route(connection) != route
    {
        effective.connection = None;
    }
    if let Some(account) = args.account {
        let provider = args
            .provider
            .as_deref()
            .ok_or_else(|| usage_message("--account requires --provider".into()))?;
        effective.connection = Some(
            crate::auth::account_id(
                &effective.account_numbers,
                &effective.connections,
                Some(crate::auth::connection_vendor(provider)),
                account.get(),
            )
            .map_err(usage_message)?
            .to_owned(),
        );
    }
    build_captured_catalogue_snapshot(args, http, &effective).await
}

/// Assemble a host's catalogue from its already captured configuration authority.
/// Saving configuration cannot rebind this input to a different instance.
pub(crate) async fn build_captured_catalogue_snapshot(
    args: &ModelsArgs,
    http: &std::sync::Arc<dyn ego_ai::http::Http>,
    effective: &ego_core::config::Effective,
) -> Result<ModelCatalogueSnapshot, RunTermination> {
    let connection = instance_discovery_connection(
        &effective.providers,
        &effective.connections,
        args.connection
            .as_deref()
            .or(effective.connection.as_deref()),
        args.provider.as_deref(),
    )
    .map_err(usage_message)?;
    let auth = &effective.auth;
    let (mut catalogue, rows) =
        configured_catalogue_rows(auth, &effective.providers, &effective.connections)
            .map_err(usage_message)?;
    let admitted = rows
        .iter()
        .map(|row| row.id().to_owned())
        .collect::<BTreeSet<_>>();
    let selected_route = connection
        .as_ref()
        .map(|(_, connection)| crate::auth::connection_route(connection));
    let provider = selected_route.as_deref().or(args.provider.as_deref());
    let selected = select_rows(rows, provider)?;

    // Filtering is settled before credentials, cache, or model facts.
    let hosted = facts::supplied(&Ceilings::default()).map_err(usage)?;
    let unsupported_sources = unsupported_supplied_sources(&hosted, &admitted, provider);
    for (slug, facts) in hosted {
        catalogue = catalogue.with_hosted(&slug, facts);
    }

    let home = config::ego_home().ok_or_else(|| RunTermination::Failed {
        message: "the model catalogue needs EGO_HOME or HOME for its durable cache".to_string(),
    })?;
    let io = OsIo;
    let cache_store = connection_cache(&io, &home, connection.as_ref().map(|(id, _)| id.as_str()));
    let (mut cache, _) = read_cache(&cache_store)?;
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
        let (latest, obsolete) = read_cache(&cache_store)?;
        cache = latest;
        let mut publish = obsolete;

        for row in &selected {
            let connection = row_connection(
                row,
                connection.as_ref(),
                &effective.providers,
                &effective.connections,
            );
            let observe = args.refresh || !cache.providers.contains_key(row.id());
            if !observe {
                continue;
            }
            let credential = match row.resolver.enumeration_credential() {
                ResolverCredentialRequirement::None => None,
                ResolverCredentialRequirement::Required => {
                    let credentials = if let Some((id, connection)) = &connection {
                        crate::auth::connection_credentials(
                            id,
                            connection,
                            row.id(),
                            &row.credential,
                            auth,
                            None,
                            http.as_ref(),
                            &crate::auth::OsCredentialCommand,
                            &CancelToken::new(),
                            None,
                        )
                        .await
                        .map_err(|error| RunTermination::Provider {
                            message: error.to_string(),
                        })?
                    } else {
                        std::sync::Arc::new(EnvCredentials)
                    };
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
                        connection.as_ref(),
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
        let connection = row_connection(
            row,
            connection.as_ref(),
            &effective.providers,
            &effective.connections,
        );
        let missing_credential = missing_enumeration_credentials
            .get(row.id())
            .map(|message| missing_credential(message.clone()))
            .or_else(|| {
                (row.resolver.enumeration_credential() == ResolverCredentialRequirement::Required
                    && !resolved_enumeration_credentials.contains(row.id()))
                .then(|| credential_for(row, connection.as_ref()))
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
    selected_connection: Option<&str>,
    selected_provider: Option<&str>,
    effective: &ego_core::config::Effective,
) -> Result<
    (
        ModelCatalogueSnapshot,
        BTreeMap<String, ego_ai::catalogue::ModelSlug>,
    ),
    RunTermination,
> {
    build_interactive_catalogue_snapshot(
        http,
        environment,
        observed,
        selected_connection,
        selected_provider,
        effective,
        false,
    )
    .await
}

pub(crate) async fn build_interactive_catalogue_snapshot(
    http: &std::sync::Arc<dyn ego_ai::http::Http>,
    environment: &dyn EnvironmentPresence,
    observed: &BTreeSet<String>,
    selected_connection: Option<&str>,
    selected_provider: Option<&str>,
    effective: &ego_core::config::Effective,
    include_fork_targets: bool,
) -> Result<
    (
        ModelCatalogueSnapshot,
        BTreeMap<String, ego_ai::catalogue::ModelSlug>,
    ),
    RunTermination,
> {
    let connection = instance_discovery_connection(
        &effective.providers,
        &effective.connections,
        selected_connection.or(effective.connection.as_deref()),
        selected_provider,
    )
    .map_err(usage_message)?;
    let auth = effective.auth.clone();
    let (mut catalogue, rows) =
        configured_catalogue_rows(&auth, &effective.providers, &effective.connections)
            .map_err(usage_message)?;
    let route = connection
        .as_ref()
        .map(|(_, connection)| crate::auth::connection_route(connection));
    // Ordinary current models do not filter the human inventory. A captured
    // named route or explicitly configured instance remains exclusive.
    let instance = selected_provider.filter(|provider| {
        effective
            .providers
            .instances
            .keys()
            .any(|id| id.as_str() == *provider)
    });
    let selected = route.as_deref().or(instance);
    let admitted = rows
        .iter()
        .filter(|row| selected.is_none_or(|id| row.id() == id))
        .map(|row| row.id().to_owned())
        .collect::<BTreeSet<_>>();
    let (hosted, defaults) = facts::supplied_with_defaults(&Ceilings::default()).map_err(usage)?;
    let mut supplied_providers = hosted
        .iter()
        .filter_map(|(slug, _)| {
            slug.split_once('/')
                .map(|(provider, _)| provider.to_owned())
        })
        .collect::<BTreeSet<_>>();
    supplied_providers.extend(
        effective
            .providers
            .instances
            .keys()
            .map(|id| id.as_str().to_owned()),
    );
    let mut sources = unsupported_supplied_sources(&hosted, &admitted, route.as_deref());
    for (slug, facts) in hosted {
        catalogue = catalogue.with_hosted(&slug, facts);
    }
    let unavailable = |row: &ProviderRow| {
        let connection = row_connection(
            row,
            connection.as_ref(),
            &effective.providers,
            &effective.connections,
        );
        mapping_unavailability(row).or_else(|| {
            if let Some((id, connection)) = &connection {
                let availability = named_availability(id, connection, environment);
                return (!matches!(availability, ModelAvailability::Available)).then_some(availability);
            }
            (!credential_present(&row.credential, environment, observed)).then(|| {
                missing_credential(format!("provider '{}' is not signed in; see 'ego auth login {}' for setup instructions", row.id(), row.id()))
            })
        })
    };
    let mut models = Vec::new();
    // A running session can display supplied facts without acquiring another
    // route's credentials, discovery results, or selection authority.
    if let Some(route) = route
        .as_deref()
        .filter(|_| selected_provider.is_some() && instance.is_none())
    {
        project_other_routes(
            OtherRoutes {
                rows: &rows,
                route,
                catalogue: &catalogue,
                supplied_providers: &supplied_providers,
                effective,
                environment,
                observed,
                selectable: include_fork_targets,
            },
            &mut models,
            &mut sources,
        )
        .map_err(internal)?;
    }
    let rows = select_rows(rows, selected)?;
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
        Some(home) => {
            read_cache(&connection_cache(
                &OsIo,
                &home,
                connection.as_ref().map(|(id, _)| id.as_str()),
            ))?
            .0
        }
        None => DiscoveryCacheV2::default(),
    };
    let mut current = BTreeSet::new();
    let mut current_sources = BTreeSet::new();
    let mut source_failures = BTreeMap::new();
    // Registry membership is not selection authority: only selected rows may discover.
    if let Some(row) = rows
        .iter()
        .find(|row| row.id() == ego_ai::catalogue::OLLAMA)
    {
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
                    message: format!("provider '{}' has no saved model list; fetch it with 'ego models --provider {} --refresh'", row.id(), row.id()),
                }),
            });
        }
    }
    Ok((
        ModelCatalogueSnapshot::from_entries_and_sources(models, sources),
        defaults,
    ))
}

/// What a seat bound to one account route may show of every other route.
struct OtherRoutes<'a> {
    rows: &'a [ProviderRow],
    route: &'a str,
    catalogue: &'a ego_ai::catalogue::Catalogue,
    supplied_providers: &'a BTreeSet<String>,
    effective: &'a ego_core::config::Effective,
    environment: &'a dyn EnvironmentPresence,
    observed: &'a BTreeSet<String>,
    /// Whether a pick may move the seat to that route's own account.
    selectable: bool,
}

/// Project the supplied facts of every other route a bound seat can reach.
///
/// A bound seat displays them without acquiring another route's credentials,
/// discovery results, or selection authority. When `selectable`, a route is
/// available exactly when one eligible account (or provider-wide credential)
/// for it exists: the account a pick of that route binds to.
fn project_other_routes(
    other: OtherRoutes<'_>,
    models: &mut Vec<ModelCatalogueEntry>,
    sources: &mut Vec<ModelCatalogueSourceStatus>,
) -> Result<(), String> {
    let OtherRoutes {
        rows,
        route,
        catalogue,
        supplied_providers,
        effective,
        environment,
        observed,
        selectable,
    } = other;
    for row in rows.iter().filter(|row| {
        row.id() != route
            && supplied_providers.contains(row.id())
            && !effective
                .providers
                .instances
                .keys()
                .any(|id| id.as_str() == row.id())
    }) {
        let report = catalogue.enumerate_hosted(row.id(), None)?;
        let availability = if selectable {
            match route_account(&effective.connections, row.id()) {
                Ok(Some((id, account))) => named_availability(&id, &account, environment),
                Ok(None) if credential_present(&row.credential, environment, observed) => {
                    ModelAvailability::Available
                }
                Ok(None) => missing_credential(format!("provider '{}' is not signed in", row.id())),
                Err(message) => missing_credential(message),
            }
        } else {
            ModelAvailability::Unavailable {
                cause: ModelAvailabilityCause::SessionAccountBound,
                message: format!(
                    "this session is bound to {route}; open a new session with 'ego -m {}/MODEL'",
                    row.id()
                ),
            }
        };
        project_supplied_with_availability(
            row,
            report.models(),
            mapping_unavailability(row).unwrap_or(availability),
            models,
            sources,
        );
    }
    Ok(())
}

/// The saved account a model on `route` runs with when no account is pinned:
/// the one account of its vendor, which must serve exactly that route.
pub(crate) fn route_account(
    connections: &BTreeMap<String, ego_core::config::ConnectionConfig>,
    route: &str,
) -> Result<Option<DiscoveryConnection>, String> {
    discovery_connection(connections, None, Some(route))
}

/// Saved facts for one ACP seat from its captured instance and account authority.
/// Never refreshes, resolves secrets, or reads another account cache. A seat
/// bound to an account route also lists every other signed-in route, which a
/// pick moves the seat to.
pub(crate) fn saved_catalogue_snapshot(
    effective: &ego_core::config::Effective,
    selected: Option<&str>,
) -> Result<ModelCatalogueSnapshot, String> {
    let provider = selected_instance_provider(effective);
    let connection = instance_discovery_connection(
        &effective.providers,
        &effective.connections,
        selected.or(effective.connection.as_deref()),
        provider,
    )?;
    let route = connection
        .as_ref()
        .map(|(_, connection)| crate::auth::connection_route(connection));
    let (mut catalogue, rows) = configured_catalogue_rows(
        &effective.auth,
        &effective.providers,
        &effective.connections,
    )?;
    let all_rows = rows.clone();
    let rows = select_rows(rows, route.as_deref()).map_err(|error| format!("{error:?}"))?;
    let hosted = facts::supplied(&Ceilings::default()).map_err(|error| error.to_string())?;
    let supplied_providers = hosted
        .iter()
        .filter_map(|(slug, _)| {
            slug.split_once('/')
                .map(|(provider, _)| provider.to_owned())
        })
        .collect::<BTreeSet<_>>();
    for (slug, facts) in hosted {
        catalogue = catalogue.with_hosted(&slug, facts);
    }
    let cache = match config::ego_home() {
        Some(home) => {
            read_cache(&connection_cache(
                &OsIo,
                &home,
                connection.as_ref().map(|(id, _)| id.as_str()),
            ))
            .map_err(|error| format!("{error:?}"))?
            .0
        }
        None => DiscoveryCacheV2::default(),
    };
    let observed = config::ego_home()
        .map(|home| {
            crate::auth::AuthStore::new(home)
                .observed_provider_ids()
                .map_err(|error| error.to_string())
        })
        .transpose()?
        .unwrap_or_default();
    let mut models = Vec::new();
    let mut sources = Vec::new();
    for row in rows {
        let connection = row_connection(
            &row,
            connection.as_ref(),
            &effective.providers,
            &effective.connections,
        );
        let availability = mapping_unavailability(&row).unwrap_or_else(|| {
            if let Some((id, connection)) = &connection {
                named_availability(id, connection, &crate::onboarding::ProcessEnvironment)
            } else if credential_present(
                &row.credential,
                &crate::onboarding::ProcessEnvironment,
                &observed,
            ) {
                ModelAvailability::Available
            } else {
                missing_credential(format!(
                    "provider '{}' has no credential in its declared source",
                    row.id()
                ))
            }
        });
        let unavailable =
            (!matches!(availability, ModelAvailability::Available)).then_some(availability);
        if let Some(slice) = cache.providers.get(row.id()) {
            project_cached(
                &row,
                slice,
                &BTreeSet::new(),
                &BTreeSet::new(),
                None,
                unavailable.as_ref(),
                &mut models,
                &mut sources,
            );
        }
        let supplied = catalogue.enumerate_hosted(row.id(), None)?;
        overlay_supplied(supplied.models(), unavailable.as_ref(), &mut models);
        if let Some(entry) = supplied.models().first() {
            sources.push(ModelCatalogueSourceStatus {
                provider: row.id().to_owned(),
                provenance: entry.provenance.clone(),
                freshness: FactFreshness::Current,
                availability: unavailable.clone().unwrap_or(ModelAvailability::Available),
            });
        }
    }
    if let Some(route) = route.as_deref() {
        project_other_routes(
            OtherRoutes {
                rows: &all_rows,
                route,
                catalogue: &catalogue,
                supplied_providers: &supplied_providers,
                effective,
                environment: &crate::onboarding::ProcessEnvironment,
                observed: &observed,
                selectable: true,
            },
            &mut models,
            &mut sources,
        )?;
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

fn read_cache(cache: &ModelCatalogueCache<'_>) -> Result<(DiscoveryCacheV2, bool), RunTermination> {
    match cache.read_bytes().map_err(internal)? {
        Some(bytes) => match DiscoveryCacheV2::decode(&bytes) {
            Ok(cache) => Ok((cache, false)),
            // Discovery facts are derived data. An older schema is a cache miss,
            // not an upgrade failure. Startup observes without publishing; the
            // ordinary catalogue path rebuilds under its existing lease.
            Err(DiscoveryCacheDecodeError::ObsoleteSchema { .. }) => {
                Ok((DiscoveryCacheV2::default(), true))
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
    connection: Option<&(String, ego_core::config::ConnectionConfig)>,
    entries: &[ego_ai::catalogue::ModelDiscoveryEntry],
    models: &mut Vec<ModelCatalogueEntry>,
    sources: &mut Vec<ModelCatalogueSourceStatus>,
) {
    // Promotion first, credential second. A row waiting on live evidence
    // cannot be run with any key, so reporting `missing_credential` for it
    // would name a fix that does not work — and reading the credential to
    // find that out would run an operator's `credential_command` for a row
    // nothing may select.
    let credential = mapping_unavailability(row).unwrap_or_else(|| credential_for(row, connection));
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

fn credential_for(
    row: &ProviderRow,
    connection: Option<&(String, ego_core::config::ConnectionConfig)>,
) -> ModelAvailability {
    if let Some((id, connection)) = connection {
        return named_availability(id, connection, &crate::onboarding::ProcessEnvironment);
    }
    let resolved = row.credential.resolve(&EnvCredentials);
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
    let mut lines = crate::render::table::plain_table(
        vec![vec![
            "SLUG".into(),
            "CONTEXT".into(),
            "OUTPUT".into(),
            "REASONING".into(),
            "METERING".into(),
            "FAST".into(),
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
                entry
                    .fast_support()
                    .map(|support| {
                        format!(
                            "{}x tokens{}",
                            support.token_price_multiplier,
                            support
                                .included_usage_multiplier
                                .map(|usage| format!(", {usage}x included usage"))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_else(|| "unsupported: no verified tier/price".into()),
                provenance(&entry.provenance),
                freshness(entry.freshness).into(),
                availability(&entry.availability),
            ]
        }))
        .collect(),
    );
    lines.push("SOURCES".to_string());
    lines.extend(crate::render::table::plain_table(
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
        ModelAvailabilityCause::SessionAccountBound => "session_account_bound",
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

/// Project the existing supplied and saved facts. This boundary has no HTTP or
/// credential port: listing cannot trigger discovery, login, or a cache write.
pub(crate) fn read_only_catalogue(
    auth: &std::collections::HashMap<String, ego_core::config::AuthProviderLayer>,
    ceilings: &Ceilings,
) -> Result<ego_core::model_catalogue::ModelCatalogue, String> {
    let effective = config::user_effective().map_err(|error| error.to_string())?;
    read_only_provider_catalogue(
        auth,
        ceilings,
        effective.connection.as_deref(),
        &effective.connections,
        None,
        &effective.providers,
    )
}

pub(crate) fn read_only_provider_catalogue(
    auth: &std::collections::HashMap<String, ego_core::config::AuthProviderLayer>,
    ceilings: &Ceilings,
    selected: Option<&str>,
    connections: &BTreeMap<String, ego_core::config::ConnectionConfig>,
    provider: Option<&str>,
    providers: &ego_core::config::ResolvedProviders,
) -> Result<ego_core::model_catalogue::ModelCatalogue, String> {
    use ego_core::model_catalogue::{CatalogueModel, CatalogueProvider, ModelCatalogue};
    let connection = instance_discovery_connection(providers, connections, selected, provider)?;
    let route = connection
        .as_ref()
        .map(|(_, connection)| crate::auth::connection_route(connection));
    let (mut catalogue, rows) = configured_catalogue_rows(auth, providers, connections)?;
    let rows = select_rows(rows, route.as_deref()).map_err(|error| format!("{error:?}"))?;
    for (slug, facts) in facts::supplied(ceilings).map_err(|error| error.to_string())? {
        catalogue = catalogue.with_hosted(&slug, facts);
    }
    let cache = match config::ego_home() {
        Some(home) => {
            read_cache(&connection_cache(
                &OsIo,
                &home,
                connection.as_ref().map(|(id, _)| id.as_str()),
            ))
            .map_err(|_| {
                "saved model catalogue is unavailable; use ego models --refresh".to_owned()
            })?
            .0
        }
        None => DiscoveryCacheV2::default(),
    };
    let configured_providers = providers;
    let mut providers = Vec::new();
    let mut models = BTreeMap::new();
    for row in &rows {
        let connection =
            row_connection(row, connection.as_ref(), configured_providers, connections);
        providers.push(CatalogueProvider {
            id: row.id().to_owned(),
            configured: (connection.as_ref().is_some_and(|(id, connection)| {
                matches!(
                    named_availability(id, connection, &crate::onboarding::ProcessEnvironment),
                    ModelAvailability::Available
                )
            }) || (connection.is_none()
                && (auth.contains_key(row.id())
                    || credential_present(
                        &row.credential,
                        &crate::onboarding::ProcessEnvironment,
                        &BTreeSet::new(),
                    ))))
            .then_some(true),
        });
        if let Some(slice) = cache.providers.get(row.id()) {
            for cached in &slice.models {
                models.insert(
                    cached.entry.slug.to_string(),
                    CatalogueModel {
                        id: cached.entry.slug.to_string(),
                        efforts: cached.entry.capabilities.reasoning_efforts.clone(),
                        selection_allowed: true,
                        availability: cached.failure.as_ref().map_or_else(
                            || "unknown".to_owned(),
                            |failure| {
                                match failure.cause {
                                    ModelDiscoveryFailureCause::SourceUnreachable => {
                                        "source_unreachable"
                                    }
                                    ModelDiscoveryFailureCause::SourceMalformed => {
                                        "source_malformed"
                                    }
                                    ModelDiscoveryFailureCause::IncompleteFacts => {
                                        "incomplete_facts"
                                    }
                                }
                                .to_owned()
                            },
                        ),
                        freshness: "cached".into(),
                        provenance: provenance(&cached.entry.provenance),
                    },
                );
            }
        }
        // This is a pure supplied-fact projection, not resolver enumeration.
        for entry in catalogue.enumerate_hosted(row.id(), None)?.models() {
            models.insert(
                entry.slug.to_string(),
                CatalogueModel {
                    id: entry.slug.to_string(),
                    efforts: entry.capabilities.reasoning_efforts.clone(),
                    selection_allowed: true,
                    availability: if mapping_unavailability(row).is_some() {
                        "mapping_unavailable"
                    } else {
                        "unknown"
                    }
                    .into(),
                    freshness: "current".into(),
                    provenance: provenance(&entry.provenance),
                },
            );
        }
    }
    providers.sort_by(|a, b| a.id.cmp(&b.id));
    let mut result = ModelCatalogue {
        snapshot: String::new(),
        effort_ceiling: None,
        providers,
        models: models.into_values().collect(),
    };
    result.snapshot = ego_core::digest::sha256_hex(
        &serde_json::to_vec(&result).map_err(|error| error.to_string())?,
    );
    Ok(result)
}

/// Adapt an already admitted picker/host snapshot without reading global state.
pub(crate) fn catalogue_snapshot_view(
    snapshot: &ModelCatalogueSnapshot,
    auth: &std::collections::HashMap<String, ego_core::config::AuthProviderLayer>,
) -> Result<ego_core::model_catalogue::ModelCatalogue, String> {
    use ego_core::model_catalogue::{CatalogueModel, CatalogueProvider, ModelCatalogue};
    let rows = model::rows(auth).map_err(|error| error.to_string())?;
    let mut providers: BTreeMap<String, CatalogueProvider> = rows
        .iter()
        .map(|row| {
            (
                row.id().to_owned(),
                CatalogueProvider {
                    id: row.id().to_owned(),
                    configured: (auth.contains_key(row.id())
                        || snapshot.models.iter().any(|entry| {
                            entry.slug.provider == row.id()
                                && entry.availability == ModelAvailability::Available
                        }))
                    .then_some(true),
                },
            )
        })
        .collect();
    for source in &snapshot.sources {
        providers
            .entry(source.provider.clone())
            .or_insert(CatalogueProvider {
                id: source.provider.clone(),
                configured: (auth.contains_key(&source.provider)
                    || snapshot.models.iter().any(|entry| {
                        entry.slug.provider == source.provider
                            && entry.availability == ModelAvailability::Available
                    }))
                .then_some(true),
            });
    }
    let mut result = ModelCatalogue {
        snapshot: String::new(),
        providers: providers.into_values().collect(),
        effort_ceiling: None,
        models: snapshot
            .models
            .iter()
            .map(|entry| CatalogueModel {
                id: entry.slug.to_string(),
                efforts: entry.capabilities.reasoning_efforts.clone(),
                selection_allowed: !matches!(
                    entry.availability,
                    ModelAvailability::Unavailable {
                        cause: ModelAvailabilityCause::SessionAccountBound,
                        ..
                    }
                ),
                availability: match &entry.availability {
                    ModelAvailability::Available => "available",
                    ModelAvailability::Unavailable { cause, .. } => availability_cause(*cause),
                }
                .into(),
                freshness: freshness(entry.freshness).into(),
                provenance: provenance(&entry.provenance),
            })
            .collect(),
    };
    result.snapshot = ego_core::digest::sha256_hex(
        &serde_json::to_vec(&result).map_err(|error| error.to_string())?,
    );
    Ok(result)
}

#[cfg(test)]
mod agent_catalogue_tests {
    use super::*;
    use ego_core::generator::types::ThinkingLevel;

    // Catches: agent metadata re-enumerating instead of using the admitted
    // picker snapshot, losing empty configured providers, or exposing errors
    // and credential-command text in model-visible results.
    #[test]
    fn model_catalogue_snapshot_view_preserves_admitted_facts_without_authentication() {
        use ego_ai::catalogue::{ModelCatalogueEntry, ModelSlug};
        let secret = "must-not-appear-in-agent-metadata";
        let auth = std::collections::HashMap::from([
            (
                "openai".into(),
                ego_core::config::AuthProviderLayer {
                    credential_command: Some(format!("/nonexistent/{secret}")),
                    ..Default::default()
                },
            ),
            (
                "anthropic".into(),
                ego_core::config::AuthProviderLayer::default(),
            ),
        ]);
        let snapshot = ModelCatalogueSnapshot::from_entries_and_sources(
            vec![ModelCatalogueEntry {
                slug: ModelSlug::parse("openai/admitted-fixture").unwrap(),
                capabilities: ego_core::generator::capabilities::ModelCapabilities {
                    schema_version:
                        ego_core::generator::capabilities::ModelCapabilities::SCHEMA_VERSION,
                    max_context_tokens: None,
                    max_output_tokens: None,
                    max_tools: None,
                    supports_parallel_tool_calls: false,
                    reasoning_carryover:
                        ego_core::generator::capabilities::ReasoningCarryover::Portable,
                    reasoning_efforts: ReasoningEffortSupport::Only(
                        [ThinkingLevel::Low, ThinkingLevel::High].into(),
                    ),
                    default_reasoning_effort: None,
                    reasoning_state_dialect: None,
                    cancel: ego_core::generator::capabilities::CancelSemantics::Cooperative,
                    cache: ego_core::generator::capabilities::CacheRetention::Short,
                    transport: ego_core::generator::capabilities::Transport::Sse,
                    image_input: ego_core::generator::capabilities::ImageInputSupport::Unknown,
                    audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
                },
                metering: ego_core::public::MeteringKind::Subscription,
                provenance: ModelFactProvenance::Discovered {
                    provider: "openai".into(),
                },
                freshness: FactFreshness::Cached,
                availability: ModelAvailability::Unavailable {
                    cause: ModelAvailabilityCause::SourceUnreachable,
                    message: secret.into(),
                },
            }],
            vec![],
        );
        let projected = catalogue_snapshot_view(&snapshot, &auth).unwrap();
        assert_eq!(
            projected.models.len(),
            1,
            "do not silently mix in other inventory"
        );
        let model = &projected.models[0];
        assert_eq!(model.id, "openai/admitted-fixture");
        assert_eq!(model.availability, "source_unreachable");
        assert_eq!(model.freshness, "cached");
        assert_eq!(model.provenance, "provider:openai");
        assert_eq!(
            model.efforts,
            ReasoningEffortSupport::Only([ThinkingLevel::Low, ThinkingLevel::High].into())
        );
        assert!(
            projected
                .providers
                .iter()
                .any(|provider| provider.id == "anthropic" && provider.configured == Some(true))
        );
        let bytes = serde_json::to_string(&projected).unwrap();
        assert!(!bytes.contains(secret));
        assert_eq!(
            projected.snapshot,
            catalogue_snapshot_view(&snapshot, &auth).unwrap().snapshot
        );
    }
}

#[cfg(test)]
mod named_discovery_tests {
    use super::*;
    use ego_ai::http::{Http, HttpCall, PreparedHttpCall};
    use ego_core::permit::{OneHopResponse, TransportError};
    use futures::future::BoxFuture;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct DiscoveryHttp(Arc<Mutex<Vec<Vec<u8>>>>);
    struct DiscoveryCall(DiscoveryHttp);
    impl Http for DiscoveryHttp {
        fn begin_call(&self) -> Box<dyn HttpCall> {
            Box::new(DiscoveryCall(self.clone()))
        }
    }
    impl HttpCall for DiscoveryCall {
        fn execute(
            &mut self,
            call: PreparedHttpCall,
        ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
            let (_, _, _, request) = call.into_parts();
            assert_eq!(request.method(), "GET");
            assert_eq!(request.path(), "/api/v1/models");
            let authorization = request
                .headers()
                .iter()
                .find(|header| header.name().eq_ignore_ascii_case("authorization"))
                .unwrap()
                .value()
                .to_vec();
            self.0.0.lock().unwrap().push(authorization);
            Box::pin(async {
                Ok(OneHopResponse::response(
                    200,
                    Vec::new(),
                    include_bytes!("../../ego-ai/tests/goldens/openrouter/models-2026-10-15.json")
                        .to_vec(),
                ))
            })
        }
    }

    fn args(id: &str, provider: &str, refresh: bool) -> ModelsArgs {
        let mut words = vec![
            "ego",
            "models",
            "--connection",
            id,
            "--provider",
            provider,
            "--json",
        ];
        if refresh {
            words.push("--refresh");
        }
        let crate::cli::Invocation::Models(args) =
            crate::cli::parse(words.into_iter().map(str::to_owned).collect()).unwrap()
        else {
            panic!("expected models request")
        };
        assert_eq!(args.connection.as_deref(), Some(id));
        *args
    }

    // Catches: explicit discovery uses the config default/ambient key or another account's cached model list.
    #[tokio::test]
    async fn named_connections_model_discovery_and_cache_follow_explicit_account() {
        const CHILD: &str = "EGO_NAMED_DISCOVERY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            std::fs::write(home.path().join("config.toml"), "connection='account_a'\n[connections.account_a]\nname='Work'\nvendor='openrouter'\naccess='key'\nkey_env='NAMED_A_KEY'\n[connections.account_b]\nname='Work'\nvendor='openrouter'\naccess='key'\nkey_env='NAMED_B_KEY'\n").unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "models::named_discovery_tests::named_connections_model_discovery_and_cache_follow_explicit_account", "--nocapture"])
                .env(CHILD, "1").env("EGO_HOME", home.path()).env("NAMED_A_KEY", "key-account-a").env("NAMED_B_KEY", "key-account-b").env("OPENROUTER_API_KEY", "wrong-ambient-key").output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let recording = DiscoveryHttp::default();
        let http: Arc<dyn Http> = Arc::new(recording.clone());
        let b = build_catalogue_snapshot(&args("account_b", "openrouter", false), &http)
            .await
            .unwrap();
        assert!(!b.models.is_empty());
        assert!(
            b.models
                .iter()
                .all(|entry| entry.slug.provider == "openrouter")
        );
        assert_eq!(
            *recording.0.lock().unwrap(),
            vec![b"Bearer key-account-b".to_vec()]
        );
        build_catalogue_snapshot(&args("account_a", "openrouter", false), &http)
            .await
            .unwrap();
        assert_eq!(
            *recording.0.lock().unwrap(),
            vec![
                b"Bearer key-account-b".to_vec(),
                b"Bearer key-account-a".to_vec()
            ]
        );
        build_catalogue_snapshot(&args("account_b", "openrouter", false), &http)
            .await
            .unwrap();
        assert_eq!(
            recording.0.lock().unwrap().len(),
            2,
            "B must reuse only its own saved snapshot"
        );
        let home = config::ego_home().unwrap();
        assert!(
            !home.join("model-catalogue/snapshot-v1.json").exists(),
            "named discovery must not publish the ambient cache"
        );
        let b_path = home.join("connections/account_b/model-catalogue/snapshot-v1.json");
        assert!(
            home.join("connections/account_a/model-catalogue/snapshot-v1.json")
                .exists()
        );
        // A pre-existing ambient snapshot is not a migration source for named discovery.
        std::fs::create_dir_all(home.join("model-catalogue")).unwrap();
        std::fs::copy(
            home.join("connections/account_a/model-catalogue/snapshot-v1.json"),
            home.join("model-catalogue/snapshot-v1.json"),
        )
        .unwrap();
        std::fs::remove_file(&b_path).unwrap();
        let effective = config::user_effective().unwrap();
        let a_metadata = read_only_provider_catalogue(
            &effective.auth,
            &Ceilings::default(),
            Some("account_a"),
            &effective.connections,
            Some("openrouter"),
            &effective.providers,
        )
        .unwrap();
        let b_metadata = read_only_provider_catalogue(
            &effective.auth,
            &Ceilings::default(),
            Some("account_b"),
            &effective.connections,
            Some("openrouter"),
            &effective.providers,
        )
        .unwrap();
        assert!(
            a_metadata
                .models
                .iter()
                .any(|entry| entry.id == "openrouter/unbiased/pareto-26.9")
        );
        assert!(
            !b_metadata
                .models
                .iter()
                .any(|entry| entry.id == "openrouter/unbiased/pareto-26.9"),
            "read-only B metadata must not borrow A's discovered facts"
        );
        // A process-wide picker snapshot must not override a resumed seat's account.
        {
            use ego_core::control::SessionControls;
            use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
            use ego_core::event::id::{EventId, Producer, Seq, SessionId, TurnId};
            use ego_core::event::time::Timestamp;
            use ego_core::store::session_store::SessionStore;
            let process_catalogue =
                build_catalogue_snapshot(&args("account_a", "openrouter", false), &http)
                    .await
                    .unwrap();
            let store = Arc::new(ego_tools::store::JsonlStore::new(
                home.join("sessions"),
                &home,
                Box::new(OsIo),
            ));
            let session = SessionId::new();
            let mut seq = Seq::ZERO;
            store
                .append(
                    &session,
                    &DomainEvent {
                        env: Envelope {
                            seq: seq.next(),
                            id: EventId::new(),
                            turn: TurnId::new(),
                            item: None,
                            parent: None,
                            ts: Timestamp::from_millis(1),
                            producer: Producer::Main,
                        },
                        kind: EventKind::SessionSelectionsSelected {
                            selections: ego_core::run::RunSelections::empty()
                                .with_connection(Some("account_b".into())),
                        },
                    },
                )
                .unwrap();
            let controls = crate::acp_controls::AcpSessionControls::new(
                &effective,
                ego_core::permit::Confinement::default(),
                Some(process_catalogue),
            )
            .with_store(store);
            controls
                .seed_profile(session, effective.clone(), None, None, None)
                .unwrap();
            controls
                .seed_confinement(session, Vec::new(), Vec::new())
                .unwrap();
            let state = controls.state(session).unwrap();
            assert!(
                !state
                    .model
                    .available
                    .iter()
                    .any(|model| model.0 == "openrouter/unbiased/pareto-26.9"),
                "resumed B picker must not reveal A's discovered models"
            );
        }
        build_catalogue_snapshot(&args("account_b", "openrouter", false), &http)
            .await
            .unwrap();
        assert_eq!(
            recording.0.lock().unwrap().last().unwrap(),
            b"Bearer key-account-b"
        );
        assert_eq!(
            recording.0.lock().unwrap().len(),
            3,
            "B's cache miss cannot borrow A's cache"
        );
        assert!(
            build_catalogue_snapshot(&args("account_b", "anthropic", false), &http)
                .await
                .is_err()
        );
        assert_eq!(
            recording.0.lock().unwrap().len(),
            3,
            "route mismatch must refuse before HTTP"
        );

        struct OnlyCustomKey;
        impl EnvironmentPresence for OnlyCustomKey {
            fn has_nonblank(&self, name: &str) -> bool {
                name == "NAMED_B_KEY"
            }
            fn is_set(&self, name: &str) -> bool {
                self.has_nonblank(name)
            }
        }
        let (startup, _) = build_startup_catalogue_snapshot(
            &http,
            &OnlyCustomKey,
            &BTreeSet::new(),
            Some("account_b"),
            Some("openrouter"),
            &effective,
        )
        .await
        .unwrap();
        assert!(!startup.models.is_empty());
        // Catches: a bound route hides Anthropic's supplied models, or exposes
        // another route as selectable in the picker or the model-facing catalogue.
        assert!(
            startup
                .models
                .iter()
                .filter(|entry| entry.availability == ModelAvailability::Available)
                .all(|entry| entry.slug.to_string().starts_with("openrouter/")),
            "the named route must remain the only selectable route"
        );
        let metadata = catalogue_snapshot_view(&startup, &effective.auth).unwrap();
        let anthropic: Vec<_> = metadata
            .models
            .iter()
            .filter(|entry| entry.id.starts_with("anthropic/"))
            .collect();
        assert!(!anthropic.is_empty(), "show Anthropic's supplied models");
        assert!(anthropic.iter().all(|entry| {
            !entry.selection_allowed && entry.availability == "session_account_bound"
        }));
        assert!(
            startup
                .models
                .iter()
                .any(|entry| matches!(entry.availability, ModelAvailability::Available))
        );
        assert!(!startup.models.iter().any(|entry| matches!(
            entry.availability,
            ModelAvailability::Unavailable {
                cause: ModelAvailabilityCause::MissingCredential,
                ..
            }
        )));
        assert_eq!(
            recording.0.lock().unwrap().len(),
            3,
            "startup must observe the selected saved cache without discovery"
        );
        let effective = config::user_effective().unwrap();
        let path = home.join("config.toml");
        std::fs::write(
            &path,
            std::fs::read_to_string(&path)
                .unwrap()
                .replace("name='Work'", "name='Renamed'")
                .replace("connection='account_a'", "connection='removed_default'"),
        )
        .unwrap();
        let metadata = read_only_provider_catalogue(
            &effective.auth,
            &Ceilings::default(),
            Some("account_b"),
            &effective.connections,
            Some("openrouter"),
            &effective.providers,
        )
        .unwrap();
        assert!(
            metadata
                .models
                .iter()
                .any(|entry| entry.id == b.models[0].slug.to_string())
        );
        build_catalogue_snapshot(&args("account_b", "openrouter", false), &http)
            .await
            .unwrap();
        assert_eq!(
            recording.0.lock().unwrap().len(),
            3,
            "rename and an overridden missing config default must keep the stable cache namespace"
        );
    }
}

#[cfg(test)]
mod provider_instance_tests {
    use super::*;
    use ego_core::config::{
        ConnectionAccess, ConnectionConfig, ProviderInstance, ProviderInstanceModel,
        ProviderModelContract, ResolvedProviders, Source, Sourced,
    };
    use ego_core::generator::capabilities::*;

    fn fixtures() -> (ResolvedProviders, BTreeMap<String, ConnectionConfig>) {
        let facts = ModelCapabilities {
            schema_version: ModelCapabilities::SCHEMA_VERSION,
            max_context_tokens: Some(32000),
            max_output_tokens: Some(2048),
            max_tools: Some(8),
            supports_parallel_tool_calls: false,
            reasoning_carryover: ReasoningCarryover::Unsupported,
            reasoning_efforts: ReasoningEffortSupport::Unknown,
            default_reasoning_effort: None,
            reasoning_state_dialect: None,
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::None,
            transport: Transport::Sse,
            image_input: ImageInputSupport::Unknown,
            audio_input: AudioInputSupport::Unknown,
        };
        let mut providers = ResolvedProviders::default();
        let mut connections = BTreeMap::new();
        for (id, host, upstream, key) in [
            (
                "office",
                "https://office.example/v1",
                "deployment-a",
                "OFFICE_API_KEY",
            ),
            (
                "home",
                "https://home.example/v1",
                "deployment-b",
                "HOME_API_KEY",
            ),
        ] {
            providers.instances.insert(
                id.parse().unwrap(),
                Sourced {
                    source: Source::User,
                    value: ProviderInstance {
                        profile: "openai-compatible".parse().unwrap(),
                        base_url: host.parse().unwrap(),
                        credential_ref: Some(id.parse().unwrap()),
                        models: BTreeMap::from([(
                            "assistant".parse().unwrap(),
                            ProviderInstanceModel {
                                upstream_id: upstream.into(),
                                contract: ProviderModelContract::ChatStandard,
                                facts: facts.clone(),
                                metering: ego_core::budget::MeteringIdentity::Currency {
                                    settlement: ego_core::budget::SettlementSource::Quote,
                                },
                            },
                        )]),
                    },
                },
            );
            connections.insert(
                id.into(),
                ConnectionConfig {
                    name: id.into(),
                    vendor: "openai".into(),
                    access: ConnectionAccess::Key,
                    key_env: Some(key.into()),
                    azure_resource: None,
                },
            );
        }
        (providers, connections)
    }

    // Catches: ordinary/host catalogue presence accepts an ApiKey in a named
    // OAuth slot even though the generation credential owner refuses that kind.
    #[test]
    fn oauth_catalogue_presence_requires_an_oauth_entry() {
        const CHILD: &str = "EGO_OAUTH_CATALOGUE_KIND_CHILD";
        const TEST: &str =
            "models::provider_instance_tests::oauth_catalogue_presence_requires_an_oauth_entry";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", home.path())
                .env("EGO_HOME", home.path())
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = config::ego_home().unwrap();
        let layer = ego_core::config::ConfigLayer::parse(
            Source::User,
            "[auth.anthropic]\noauth_login_enabled=true\n[connections.account]\nname='Account'\nvendor='anthropic'\naccess='oauth'\n",
        )
        .unwrap();
        let effective =
            ego_core::config::resolve(&[layer], ego_core::config::Trust::Untrusted, None).unwrap();
        let connection = &effective.connections["account"];
        let store = crate::auth::AuthStore::new(&home);
        for (entry, expected) in [
            (None, false),
            (
                Some(crate::auth::AuthEntry::ApiKey {
                    key: "synthetic-wrong-kind-key".into(),
                }),
                false,
            ),
            (
                Some(crate::auth::AuthEntry::OAuth {
                    access: "synthetic-oauth-token".into(),
                    refresh: None,
                    expires: None,
                    scope_binding: None,
                }),
                true,
            ),
        ] {
            if let Some(entry) = entry {
                store.put("connection:account", entry).unwrap();
            }
            assert_eq!(
                matches!(
                    named_availability(
                        "account",
                        connection,
                        &crate::onboarding::ProcessEnvironment
                    ),
                    ModelAvailability::Available
                ),
                expected,
                "ordinary model availability must honor the declared OAuth kind"
            );
            let catalogue = read_only_provider_catalogue(
                &effective.auth,
                &Ceilings::default(),
                Some("account"),
                &effective.connections,
                Some("anthropic"),
                &effective.providers,
            )
            .unwrap();
            assert_eq!(
                catalogue
                    .providers
                    .iter()
                    .find(|row| row.id == "anthropic")
                    .unwrap()
                    .configured,
                expected.then_some(true),
                "read-only host catalogue must honor the declared OAuth kind"
            );
        }
    }

    // Catches: configured instances claim ambient-key availability instead of
    // checking the exact stored-only credential_ref across listings and setup.
    #[tokio::test]
    async fn stored_instance_availability_uses_exact_named_key() {
        const CHILD: &str = "EGO_STORED_INSTANCE_CHILD";
        const TEST: &str =
            "models::provider_instance_tests::stored_instance_availability_uses_exact_named_key";
        if std::env::var_os(CHILD).is_none() {
            for ambient in [false, true] {
                let home = tempfile::tempdir().unwrap();
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", TEST, "--nocapture"])
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .env("HOME", home.path())
                    .env("EGO_HOME", home.path())
                    .env(CHILD, "1");
                if ambient {
                    command.env("OPENAI_API_KEY", "synthetic-wrong-ambient");
                }
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let home = config::ego_home().unwrap();
        let (providers, mut connections) = fixtures();
        for connection in connections.values_mut() {
            connection.key_env = None;
        }
        let mut effective =
            ego_core::config::resolve(&[], ego_core::config::Trust::Untrusted, None).unwrap();
        effective.providers = providers;
        effective.connections = connections;
        let args = ModelsArgs {
            account: None,
            connection: None,
            provider: Some("office".into()),
            refresh: false,
            json: true,
        };
        let http: std::sync::Arc<dyn ego_ai::http::Http> = std::sync::Arc::new(NoDiscovery);
        let store = crate::auth::AuthStore::new(&home);
        store
            .put(
                "connection:home",
                crate::auth::AuthEntry::ApiKey {
                    key: "synthetic-other-key".into(),
                },
            )
            .unwrap();
        for present in [false, true] {
            if present {
                store
                    .put(
                        "connection:office",
                        crate::auth::AuthEntry::ApiKey {
                            key: "synthetic-exact-key".into(),
                        },
                    )
                    .unwrap();
            }
            let captured = build_captured_catalogue_snapshot(&args, &http, &effective)
                .await
                .unwrap();
            let (startup, _) = build_startup_catalogue_snapshot(
                &http,
                &NoEnvironment,
                &BTreeSet::new(),
                None,
                Some("office"),
                &effective,
            )
            .await
            .unwrap();
            let saved = saved_catalogue_snapshot(&effective, None).unwrap();
            let metadata = read_only_provider_catalogue(
                &effective.auth,
                &Ceilings::default(),
                None,
                &effective.connections,
                Some("office"),
                &effective.providers,
            )
            .unwrap();
            let readiness = crate::onboarding::captured_provider_readiness(&home, &effective)
                .unwrap()
                .into_iter()
                .find(|provider| provider.provider == "office")
                .unwrap();
            assert_eq!(readiness.ready, present, "{readiness}");
            let expected = if present {
                "office: ready"
            } else {
                "office: not ready — configure credentials for connection 'office'"
            };
            assert_eq!(readiness.to_string(), expected);
            assert_eq!(
                metadata
                    .providers
                    .iter()
                    .find(|provider| provider.id == "office")
                    .unwrap()
                    .configured,
                present.then_some(true)
            );
            for snapshot in [captured, startup, saved] {
                let models: Vec<_> = snapshot
                    .models
                    .iter()
                    .filter(|entry| entry.slug.provider == "office")
                    .collect();
                assert!(!models.is_empty());
                assert!(
                    models.iter().all(|entry| matches!(
                        entry.availability,
                        ModelAvailability::Available
                    ) == present),
                    "{snapshot:?}"
                );
            }
        }
    }

    struct NoDiscovery;

    impl ego_ai::http::Http for NoDiscovery {
        fn begin_call(&self) -> Box<dyn ego_ai::http::HttpCall> {
            panic!("configured supplied instances must not perform discovery")
        }
    }

    struct NoEnvironment;

    impl EnvironmentPresence for NoEnvironment {
        fn has_nonblank(&self, _: &str) -> bool {
            false
        }
        fn is_set(&self, _: &str) -> bool {
            false
        }
    }

    // Catches a second config capture rebinding existing CLI/ACP/MCP/picker/setup
    // projections, an instance alias entering account auto-selection, and filtered
    // startup discovery reaching an unrelated local provider.
    #[tokio::test]
    async fn captured_instance_host_projections_survive_saved_replacement() {
        use ego_core::config::{ModelRef, Trust};
        use ego_core::control::{SessionControlChange, SessionControls};
        use ego_core::generator::types::ThinkingLevel;
        use std::sync::Arc;

        const CHILD: &str = "EGO_CAPTURED_INSTANCE_HOST_CHILD";
        const TEST: &str = "models::provider_instance_tests::captured_instance_host_projections_survive_saved_replacement";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", home.path())
                .env("EGO_HOME", home.path())
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(&format!("test {TEST} ... ok"))
            );
            return;
        }
        let home = config::ego_home().unwrap();
        let (providers, _) = fixtures();
        let mut spec = providers.instances.values().next().unwrap().value.clone();
        spec.base_url = "http://127.0.0.1:48888/v1/".parse().unwrap();
        spec.credential_ref = None;
        let model = spec.models.values_mut().next().unwrap();
        model.upstream_id = "deployment-a".into();
        model.facts.reasoning_efforts =
            ReasoningEffortSupport::Only([ThinkingLevel::Low, ThinkingLevel::High].into());
        model.facts.default_reasoning_effort = Some(ThinkingLevel::Low);
        // A second selectable alias proves ACP does not merely repeat its current model.
        spec.models.insert(
            "other".parse().unwrap(),
            spec.models.values().next().unwrap().clone(),
        );
        let http: Arc<dyn ego_ai::http::Http> = Arc::new(NoDiscovery);
        let args = ModelsArgs {
            account: None,
            connection: None,
            provider: Some("office".into()),
            refresh: false,
            json: true,
        };
        let instance_config = |spec: &ProviderInstance| {
            toml::to_string(&BTreeMap::from([(
                "providers",
                BTreeMap::from([("instances", BTreeMap::from([("office", spec)]))]),
            )]))
            .unwrap()
        };
        for explicit in [false, true] {
            let thinking = if explicit { "thinking = 'high'\n" } else { "" };
            let write =
                |spec: &ProviderInstance| {
                    std::fs::write(home.join("config.toml"), format!(
                    "model = 'office/assistant'\nbudget = '1k'\nsandbox = 'off'\n{thinking}{}",
                    instance_config(spec),
                )).unwrap();
                };
            write(&spec);
            let captured = config::user_effective().unwrap();
            let protocol_args =
                crate::run::protocol_args(None, crate::cli::SessionShape::default());
            let frozen = Arc::new(
                config::FrozenRunConfig::capture(&home, &protocol_args, Trust::Untrusted).unwrap(),
            );
            let initial = build_captured_catalogue_snapshot(&args, &http, &captured)
                .await
                .unwrap();
            assert_eq!(initial.models.len(), 2);
            assert_eq!(initial.models[0].slug.to_string(), "office/assistant");
            assert_eq!(initial.models[0].capabilities.max_output_tokens, Some(2048));
            assert_eq!(initial.models[0].availability, ModelAvailability::Available);
            let controls = crate::acp_controls::AcpSessionControls::new(
                &captured,
                ego_core::permit::Confinement::Off,
                Some(initial.clone()),
            );
            let session = ego_core::event::id::SessionId::new();
            controls
                .seed_profile(
                    session,
                    captured.clone(),
                    None,
                    None,
                    Some(Arc::clone(&frozen)),
                )
                .unwrap();
            controls
                .seed_confinement(session, Vec::new(), Vec::new())
                .unwrap();

            let mut replacement = spec.clone();
            replacement.base_url = "http://127.0.0.1:49999/v1/".parse().unwrap();
            for model in replacement.models.values_mut() {
                model.upstream_id = "deployment-b".into();
                model.facts.max_output_tokens = Some(4096);
            }
            write(&replacement);
            let saved_text = std::fs::read_to_string(home.join("config.toml")).unwrap();
            std::fs::write(
                home.join("config.toml"),
                saved_text.replace("model = 'office/assistant'", "model = 'office/other'"),
            )
            .unwrap();
            let old = build_captured_catalogue_snapshot(&args, &http, &captured)
                .await
                .unwrap();
            let (startup, _) = build_startup_catalogue_snapshot(
                &http,
                &NoEnvironment,
                &BTreeSet::new(),
                None,
                Some("office"),
                &captured,
            )
            .await
            .unwrap();
            assert_eq!(startup.models.len(), 2);
            let saved = saved_catalogue_snapshot(&captured, None).unwrap();
            for snapshot in [&old, &startup, &saved] {
                let entry = snapshot
                    .models
                    .iter()
                    .find(|entry| entry.slug.to_string() == "office/assistant")
                    .unwrap();
                assert_eq!(entry.capabilities.max_output_tokens, Some(2048));
                assert_eq!(entry.availability, ModelAvailability::Available);
                let metadata = catalogue_snapshot_view(snapshot, &captured.auth).unwrap();
                assert!(
                    metadata.providers.iter().any(
                        |provider| provider.id == "office" && provider.configured == Some(true)
                    )
                );
                assert!(
                    metadata
                        .models
                        .iter()
                        .any(|model| model.id == "office/other")
                );
                assert!(
                    !serde_json::to_string(&metadata)
                        .unwrap()
                        .contains("127.0.0.1")
                );
            }
            let readiness =
                crate::onboarding::captured_provider_readiness(&home, &captured).unwrap();
            assert!(
                readiness
                    .iter()
                    .any(|provider| provider.provider == "office" && provider.ready)
            );
            let pinned = controls
                .profile_effective(session, None, None)
                .unwrap()
                .unwrap();
            assert_eq!(pinned.providers, captured.providers);
            assert!(
                controls
                    .state(session)
                    .unwrap()
                    .model
                    .available
                    .contains(&ModelRef("office/other".into()))
            );
            let selected = controls
                .stage(
                    session,
                    SessionControlChange::Model(ModelRef("office/other".into())),
                )
                .unwrap();
            assert_eq!(
                selected.model.current,
                Some(ModelRef("office/other".into()))
            );

            let store = Arc::new(ego_tools::store::JsonlStore::new(
                home.join("sessions"),
                home.clone(),
                Box::new(OsIo),
            ));
            let artifacts: Arc<dyn ego_core::store::run_artifact::RunArtifactStore> = store.clone();
            let (_, policy) = crate::run::CliRunExecutor::for_mcp_captured(
                ego_core::run::RunRoot::new(home.clone()).unwrap(),
                None,
                store,
                artifacts,
                std::num::NonZeroUsize::new(1).unwrap(),
                Arc::new(ego_core::evidence::EvidenceAdmissionService::new()),
                if explicit { None } else { Some(&initial) },
                frozen,
            )
            .unwrap();
            assert_eq!(policy.model.0, "office/assistant");
            assert_eq!(
                policy.thinking,
                if explicit {
                    ThinkingLevel::High
                } else {
                    ThinkingLevel::Low
                }
            );
            let (old_catalogue, _) = configured_catalogue_rows(
                &captured.auth,
                &captured.providers,
                &captured.connections,
            )
            .unwrap();
            assert_eq!(
                old_catalogue.row("office").unwrap().base().port(),
                Some(48888)
            );
            assert_eq!(
                old_catalogue
                    .static_request_profile(
                        &ego_ai::catalogue::ModelSlug::parse("office/assistant").unwrap()
                    )
                    .unwrap()
                    .unwrap()
                    .request_model(),
                "deployment-a"
            );
            let fresh = build_catalogue_snapshot(&args, &http).await.unwrap();
            assert_eq!(fresh.models[0].capabilities.max_output_tokens, Some(4096));
        }
        // Invalid binding refuses at each shared assembler before the HTTP port.
        spec.credential_ref = Some("missing".parse().unwrap());
        let layer = ego_core::config::ConfigLayer::parse(
            Source::User,
            &format!("model='office/assistant'\n{}", instance_config(&spec),),
        )
        .unwrap();
        let invalid = ego_core::config::resolve(&[layer], Trust::Untrusted, None).unwrap();
        assert!(
            build_captured_catalogue_snapshot(&args, &http, &invalid)
                .await
                .is_err()
        );
        assert!(
            build_startup_catalogue_snapshot(
                &http,
                &NoEnvironment,
                &BTreeSet::new(),
                None,
                Some("office"),
                &invalid
            )
            .await
            .is_err()
        );
        assert!(saved_catalogue_snapshot(&invalid, None).is_err());
        assert!(crate::onboarding::captured_provider_readiness(&home, &invalid).is_err());

        // A valid but absent declared key must not become an available local row.
        let mut unauthenticated = invalid;
        unauthenticated.connections = fixtures().1;
        unauthenticated
            .providers
            .instances
            .values_mut()
            .next()
            .unwrap()
            .value
            .credential_ref = Some("office".parse().unwrap());
        let missing = build_captured_catalogue_snapshot(&args, &http, &unauthenticated)
            .await
            .unwrap();
        assert!(missing.models.iter().all(|entry| matches!(
            entry.availability,
            ModelAvailability::Unavailable {
                cause: ModelAvailabilityCause::MissingCredential,
                ..
            }
        )));
        let readiness =
            crate::onboarding::captured_provider_readiness(&home, &unauthenticated).unwrap();
        assert!(
            readiness
                .iter()
                .any(|provider| provider.provider == "office" && !provider.ready)
        );
    }

    // Catches account auto-selection hiding two instance rows or redirecting their exact model aliases.
    #[test]
    fn provider_instances_share_the_model_listing_authority_without_account_rebinding() {
        let (providers, connections) = fixtures();
        assert!(
            instance_discovery_connection(&providers, &connections, None, None)
                .unwrap()
                .is_none()
        );
        assert!(
            instance_discovery_connection(&providers, &connections, Some("office"), Some("office"))
                .unwrap()
                .is_none()
        );
        assert!(
            instance_discovery_connection(&providers, &connections, Some("home"), Some("office"))
                .is_err()
        );
        let (catalogue, rows) =
            configured_catalogue_rows(&Default::default(), &providers, &connections).unwrap();
        let selected = select_rows(rows, Some("office")).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id(), "office");
        let office = catalogue.enumerate_hosted("office", None).unwrap();
        let home = catalogue.enumerate_hosted("home", None).unwrap();
        assert_eq!(office.models().len(), 1);
        assert_eq!(home.models().len(), 1);
        assert_eq!(office.models()[0].slug.to_string(), "office/assistant");
        assert_eq!(home.models()[0].slug.to_string(), "home/assistant");
        assert_eq!(
            catalogue
                .static_request_profile(&office.models()[0].slug)
                .unwrap()
                .unwrap()
                .request_model(),
            "deployment-a"
        );
        assert_eq!(
            catalogue
                .static_request_profile(&home.models()[0].slug)
                .unwrap()
                .unwrap()
                .request_model(),
            "deployment-b"
        );
    }
}
