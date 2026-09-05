//! Process-wide MCP management through one live registry authority.
//!
//! Configuration mutation remains core's atomic transaction. Admission,
//! health, authorization, and captured inventory are projected directly from
//! the controller supplied by process composition; this module owns no second
//! reducer or cached protocol state.

use std::sync::Arc;

use ego_core::config::{
    McpConfigProvenance, McpHttpAuth, McpServerConfig, McpServerName, ProfileName,
    ResolvedMcpEntry, Source, UserConfigMutation, UserConfigMutationResult, UserConfigPublication,
    UserConfigStore, UserConfigTarget,
};
use ego_core::run::RunTermination;
use ego_mcp::{
    McpCacheFreshness, McpFailure, McpRegistryController, McpRegistryRefresh, McpRegistryStatus,
    McpServerState, McpServerStatus, RegistryScope, RegistryTrigger,
};
use ego_tools::config::OsUserConfigStore;
use ego_tools::store::OsIo;
use serde::Serialize;

use crate::cli::{McpAction, McpArgs, UsageError};
use crate::config::{self, McpManagementConfig};
use crate::output::{Diagnostics, Stdout, empty_listing};

/// Execute MCP management through the process's sole live registry authority.
pub async fn execute(
    args: &McpArgs,
    out: &Stdout,
    controller: &Arc<McpRegistryController>,
) -> RunTermination {
    match execute_inner(args, out, controller).await {
        Ok(()) => RunTermination::Done,
        Err(termination) => termination,
    }
}

async fn execute_inner(
    args: &McpArgs,
    out: &Stdout,
    controller: &Arc<McpRegistryController>,
) -> Result<(), RunTermination> {
    match &args.action {
        McpAction::Add { name, server } => add(name, server, out, controller).await,
        McpAction::Remove { name } => remove(name, out, controller).await,
        McpAction::List { json } => list(*json, out, controller).await,
        McpAction::Login { name } => login(name),
        McpAction::Reload { name } => reload(name.as_ref(), out, controller).await,
    }
}

fn login(_name: &McpServerName) -> Result<(), RunTermination> {
    Err(RunTermination::Failed {
        message: "MCP login is deployment-unavailable: the project CIMD client metadata origin and loopback redirect have not been published"
            .to_owned(),
    })
}

async fn reload(
    name: Option<&McpServerName>,
    out: &Stdout,
    controller: &Arc<McpRegistryController>,
) -> Result<(), RunTermination> {
    let config = management_config()?;
    if let Some(name) = name
        && config.effective.mcp.configured(name).is_none()
    {
        return Err(usage(UsageError(format!(
            "MCP server `{name}` is not configured"
        ))));
    }
    let scope = name
        .cloned()
        .map_or(RegistryScope::All, RegistryScope::Server);
    let status = reconcile(&config, controller, RegistryTrigger::Reload, scope).await?;
    render_status(&status, out);
    Ok(())
}

fn management_config() -> Result<McpManagementConfig, RunTermination> {
    config::mcp_management().map_err(usage)
}

fn usage(error: UsageError) -> RunTermination {
    RunTermination::UsageError {
        message: error.to_string(),
    }
}

fn publish(
    config: &McpManagementConfig,
    mutation: UserConfigMutation,
) -> Result<UserConfigMutationResult, RunTermination> {
    let store = OsUserConfigStore::new(Arc::new(OsIo), config.user_path.as_path());
    store
        .mutate(mutation)
        .map_err(|error| RunTermination::Failed {
            message: format!("user MCP configuration was not published: {error}"),
        })
}

async fn add(
    name: &McpServerName,
    server: &McpServerConfig,
    out: &Stdout,
    controller: &Arc<McpRegistryController>,
) -> Result<(), RunTermination> {
    let before = management_config()?;
    let target = before.selected_user_target();
    let result = publish(
        &before,
        UserConfigMutation::McpSet {
            target: target.clone(),
            name: name.clone(),
            expected_user_entry: before.user_entry(&target, name).cloned(),
            server: server.clone(),
        },
    )?;
    let publication = result.publication;
    let after = resolve_after_publication(&publication)?;
    let status = reconcile(
        &after,
        controller,
        RegistryTrigger::Add,
        RegistryScope::Server(name.clone()),
    )
    .await?;
    out.line(&format!(
        "{}; set MCP server `{name}`; {}; {}",
        publication_text(&publication),
        effective_text(&after, name),
        server_status_text(&status, name)
    ));
    Ok(())
}

#[derive(Clone, Copy)]
enum Removal {
    Entry,
    Tombstone,
    SystemMask,
}

async fn remove(
    name: &McpServerName,
    out: &Stdout,
    controller: &Arc<McpRegistryController>,
) -> Result<(), RunTermination> {
    let before = management_config()?;
    let (mutation, removal) = removal_mutation(&before, name)?;
    let result = publish(&before, mutation)?;
    let publication = result.publication;
    let after = resolve_after_publication(&publication)?;
    let status = reconcile(
        &after,
        controller,
        RegistryTrigger::Remove,
        RegistryScope::Server(name.clone()),
    )
    .await?;
    let consequence = match removal {
        Removal::Entry => format!("removed user entry; {}", effective_text(&after, name)),
        Removal::Tombstone => format!("removed user tombstone; {}", reveal_text(&after, name)),
        Removal::SystemMask => format!("masked system entry; {}", effective_text(&after, name)),
    };
    out.line(&format!(
        "{}; {consequence}; {}",
        publication_text(&publication),
        server_status_text(&status, name)
    ));
    Ok(())
}

fn removal_mutation(
    config: &McpManagementConfig,
    name: &McpServerName,
) -> Result<(UserConfigMutation, Removal), RunTermination> {
    let Some(entry) = config.effective.mcp.entry(name) else {
        return Err(usage(UsageError(format!(
            "MCP server `{name}` is not configured"
        ))));
    };
    let (provenance, removal) = match entry {
        ResolvedMcpEntry::Configured { provenance, .. } => (provenance, Removal::Entry),
        ResolvedMcpEntry::Tombstone { provenance } => (provenance, Removal::Tombstone),
    };
    match provenance {
        McpConfigProvenance::Layer {
            source: Source::User,
        } => delete_user_entry(config, UserConfigTarget::Base, name, removal),
        McpConfigProvenance::Profile {
            source: Source::User,
            name: profile,
        } => delete_user_entry(
            config,
            UserConfigTarget::Profile(ProfileName::parse(profile).map_err(|error| {
                usage(UsageError(format!(
                    "resolved user profile has an invalid name: {error}"
                )))
            })?),
            name,
            removal,
        ),
        McpConfigProvenance::Layer {
            source: Source::System,
        }
        | McpConfigProvenance::Profile {
            source: Source::System,
            ..
        } => {
            if matches!(removal, Removal::Tombstone) {
                return Err(usage(UsageError(format!(
                    "MCP server `{name}` is not configured; the system layer contains a tombstone"
                ))));
            }
            let expected_user_entry = config.user_entry(&UserConfigTarget::Base, name).cloned();
            if expected_user_entry.is_some() {
                return Err(RunTermination::Failed {
                    message: format!(
                        "resolved system MCP server `{name}` conflicts with an observed user entry"
                    ),
                });
            }
            Ok((
                UserConfigMutation::McpInsertUserTombstone {
                    name: name.clone(),
                    expected_user_entry,
                },
                Removal::SystemMask,
            ))
        }
        McpConfigProvenance::Layer { source } | McpConfigProvenance::Profile { source, .. } => {
            Err(usage(UsageError(format!(
                "MCP server `{name}` is configured by the {source} layer, which user configuration cannot mask"
            ))))
        }
    }
}

fn delete_user_entry(
    config: &McpManagementConfig,
    target: UserConfigTarget,
    name: &McpServerName,
    removal: Removal,
) -> Result<(UserConfigMutation, Removal), RunTermination> {
    let expected_entry =
        config
            .user_entry(&target, name)
            .cloned()
            .ok_or_else(|| RunTermination::Failed {
                message: format!(
                    "resolved user MCP server `{name}` is absent from its provenance target"
                ),
            })?;
    Ok((
        UserConfigMutation::McpDeleteUserEntry {
            target,
            name: name.clone(),
            expected_entry,
        },
        removal,
    ))
}

fn resolve_after_publication(
    publication: &UserConfigPublication,
) -> Result<McpManagementConfig, RunTermination> {
    config::mcp_management().map_err(|error| RunTermination::Failed {
        message: format!(
            "{} but the resulting MCP authority could not be resolved: {}",
            publication_text(publication),
            error
        ),
    })
}

fn publication_text(publication: &UserConfigPublication) -> String {
    match publication {
        UserConfigPublication::NotPublished => "unchanged; not published".to_string(),
        UserConfigPublication::PublishedDurably => "published durably".to_string(),
        UserConfigPublication::PublishedDurabilityUncertain { diagnostic } => {
            format!("published with durability uncertain: {diagnostic}")
        }
    }
}

fn effective_text(config: &McpManagementConfig, name: &McpServerName) -> String {
    match config.effective.mcp.entry(name) {
        Some(ResolvedMcpEntry::Configured { provenance, .. }) => {
            format!("configured from {}", provenance_text(provenance))
        }
        Some(ResolvedMcpEntry::Tombstone { provenance }) => {
            format!("not configured (masked by {})", provenance_text(provenance))
        }
        None => "not configured".to_string(),
    }
}

fn reveal_text(config: &McpManagementConfig, name: &McpServerName) -> String {
    match config.effective.mcp.entry(name) {
        Some(ResolvedMcpEntry::Configured { provenance, .. }) => {
            format!("revealed {}", provenance_text(provenance))
        }
        Some(ResolvedMcpEntry::Tombstone { provenance }) => {
            format!("still masked by {}", provenance_text(provenance))
        }
        None => "no lower entry was revealed".to_string(),
    }
}

fn provenance_text(provenance: &McpConfigProvenance) -> String {
    match provenance {
        McpConfigProvenance::Layer { source } => format!("{source} base"),
        McpConfigProvenance::Profile { source, name } => {
            format!("{source} profile `{name}`")
        }
    }
}

async fn reconcile(
    config: &McpManagementConfig,
    controller: &Arc<McpRegistryController>,
    trigger: RegistryTrigger,
    scope: RegistryScope,
) -> Result<McpRegistryStatus, RunTermination> {
    let admissions =
        ego_tools::mcp::materialize_admissions(config::configured_mcp(&config.effective));
    let refresh = McpRegistryRefresh::new(admissions, trigger, scope).map_err(|error| {
        RunTermination::Failed {
            message: error.to_string(),
        }
    })?;
    controller
        .reconcile(refresh)
        .await
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
    Ok(controller.status())
}

fn render_status(status: &McpRegistryStatus, out: &Stdout) {
    if status.servers().is_empty() {
        out.line("no MCP servers are admitted");
    } else {
        for server in status.servers() {
            out.line(&format!(
                "{}\t{}",
                server.name(),
                server_state_text(server.state())
            ));
        }
    }
}

fn server_status_text(status: &McpRegistryStatus, name: &McpServerName) -> &'static str {
    status
        .server(name)
        .map(|server| server_state_text(server.state()))
        .unwrap_or("absent")
}

fn server_state_text(state: McpServerState) -> &'static str {
    match state {
        McpServerState::Admitted => "admitted",
        McpServerState::Degraded => "degraded",
        McpServerState::AuthorizationRequired => "authorization_required",
        McpServerState::Absent => "absent",
    }
}

async fn list(
    json: bool,
    out: &Stdout,
    controller: &Arc<McpRegistryController>,
) -> Result<(), RunTermination> {
    let config = management_config()?;
    let status = reconcile(
        &config,
        controller,
        RegistryTrigger::ConfigChanged,
        RegistryScope::All,
    )
    .await?;
    let listing = Listing::from_config(&config, &status);
    if json {
        let rendered = serde_json::to_string(&listing).map_err(|error| RunTermination::Failed {
            message: format!("the MCP configuration listing could not be rendered: {error}"),
        })?;
        out.line(&rendered);
    } else if listing.servers.is_empty() {
        // No rows, so nothing belongs on stdout; the explanation is a
        // diagnostic, exactly as it is for every other empty listing.
        out.note(&empty_listing(
            "no MCP servers are configured or admitted",
            "ego mcp add",
        ));
    } else {
        for server in listing.servers {
            let state = if server.configured {
                "configured"
            } else {
                "masked"
            };
            let diagnostic = server
                .diagnostic
                .as_ref()
                .map(|diagnostic| format!("\t{}: {}", diagnostic.class, diagnostic.message))
                .unwrap_or_default();
            out.line(&format!(
                "{}\t{state}\t{}\t{}{diagnostic}",
                server.name,
                server.provenance.text(),
                server.health,
            ));
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct Listing {
    servers: Vec<ServerReport>,
}

impl Listing {
    fn from_config(config: &McpManagementConfig, status: &McpRegistryStatus) -> Self {
        let servers = config
            .effective
            .mcp
            .entries()
            .iter()
            .map(|(name, entry)| ServerReport::new(name, entry, status))
            .collect();
        Self { servers }
    }
}

#[derive(Serialize)]
struct ServerReport {
    name: String,
    configured: bool,
    admitted: bool,
    health: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<CapabilitiesReport>,
    cache: CacheReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorization: Option<AuthorizationReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic: Option<DiagnosticReport>,
    provenance: ProvenanceReport,
    configuration: Option<ConfigurationReport>,
}

impl ServerReport {
    fn new(name: &McpServerName, entry: &ResolvedMcpEntry, status: &McpRegistryStatus) -> Self {
        let health = status
            .server(name)
            .map(|server| server_state_text(server.state()))
            .unwrap_or("absent");
        let projection = RegistryStatusReport::new(status.server(name));
        match entry {
            ResolvedMcpEntry::Configured { server, provenance } => Self {
                name: name.to_string(),
                configured: true,
                admitted: matches!(
                    status.server(name).map(|server| server.state()),
                    Some(McpServerState::Admitted)
                ),
                health,
                generation: projection.generation,
                capabilities: projection.capabilities,
                cache: projection.cache,
                authorization: projection.authorization,
                diagnostic: projection.diagnostic,
                provenance: ProvenanceReport::new(provenance),
                configuration: Some(ConfigurationReport::new(server)),
            },
            ResolvedMcpEntry::Tombstone { provenance } => Self {
                name: name.to_string(),
                configured: false,
                admitted: false,
                health,
                generation: projection.generation,
                capabilities: projection.capabilities,
                cache: projection.cache,
                authorization: projection.authorization,
                diagnostic: projection.diagnostic,
                provenance: ProvenanceReport::new(provenance),
                configuration: None,
            },
        }
    }
}

struct RegistryStatusReport {
    generation: Option<String>,
    capabilities: Option<CapabilitiesReport>,
    cache: CacheReport,
    authorization: Option<AuthorizationReport>,
    diagnostic: Option<DiagnosticReport>,
}

impl RegistryStatusReport {
    fn new(status: Option<&McpServerStatus>) -> Self {
        Self {
            generation: status
                .and_then(McpServerStatus::generation)
                .map(str::to_owned),
            capabilities: status
                .and_then(McpServerStatus::capabilities)
                .map(CapabilitiesReport::new),
            cache: CacheReport {
                freshness: status
                    .map(|server| cache_freshness_text(server.cache_freshness()))
                    .unwrap_or("uncached"),
                source_revision: status
                    .and_then(McpServerStatus::cache_source_revision)
                    .map(str::to_owned),
            },
            authorization: status
                .and_then(McpServerStatus::authorization)
                .map(|authorization| AuthorizationReport {
                    issuer: authorization.issuer().to_owned(),
                    protected_resource: authorization.protected_resource().to_owned(),
                    scopes: authorization.scopes().to_vec(),
                    request_correlation: authorization.request_correlation().to_owned(),
                }),
            diagnostic: status
                .and_then(McpServerStatus::diagnostic)
                .map(|diagnostic| DiagnosticReport {
                    class: failure_text(diagnostic.failure()),
                    message: diagnostic.message().to_owned(),
                }),
        }
    }
}

#[derive(Serialize)]
struct CapabilitiesReport {
    tools: bool,
    resources: bool,
    prompts: bool,
}

impl CapabilitiesReport {
    fn new(capabilities: &ego_mcp::McpCapabilities) -> Self {
        Self {
            tools: capabilities.has_tools(),
            resources: capabilities.has_resources(),
            prompts: capabilities.has_prompts(),
        }
    }
}

#[derive(Serialize)]
struct CacheReport {
    freshness: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_revision: Option<String>,
}

#[derive(Serialize)]
struct AuthorizationReport {
    issuer: String,
    protected_resource: String,
    scopes: Vec<String>,
    request_correlation: String,
}

#[derive(Serialize)]
struct DiagnosticReport {
    class: &'static str,
    message: String,
}

fn cache_freshness_text(freshness: McpCacheFreshness) -> &'static str {
    match freshness {
        McpCacheFreshness::Fresh => "fresh",
        McpCacheFreshness::Expired => "expired",
        McpCacheFreshness::Uncached => "uncached",
    }
}

fn failure_text(failure: McpFailure) -> &'static str {
    match failure {
        McpFailure::Absent => "absent",
        McpFailure::InvalidValue => "invalid_value",
        McpFailure::InvalidResponse => "invalid_response",
        McpFailure::Authority => "authority",
        McpFailure::Transport => "transport",
        McpFailure::Protocol => "protocol",
        McpFailure::Authorization => "authorization",
        McpFailure::Credential => "credential",
        McpFailure::Cancelled => "cancelled",
    }
}

#[derive(Serialize)]
struct ProvenanceReport {
    source: String,
    slot: &'static str,
    profile: Option<String>,
}

impl ProvenanceReport {
    fn new(provenance: &McpConfigProvenance) -> Self {
        match provenance {
            McpConfigProvenance::Layer { source } => Self {
                source: source.to_string(),
                slot: "base",
                profile: None,
            },
            McpConfigProvenance::Profile { source, name } => Self {
                source: source.to_string(),
                slot: "profile",
                profile: Some(name.clone()),
            },
        }
    }

    fn text(&self) -> String {
        match &self.profile {
            Some(profile) => format!("{} profile `{profile}`", self.source),
            None => format!("{} base", self.source),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "transport", rename_all = "lowercase")]
enum ConfigurationReport {
    Stdio {
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
        inherit_env: Vec<String>,
    },
    Http {
        url: String,
        auth: &'static str,
        allow_insecure_loopback: bool,
    },
}

impl ConfigurationReport {
    fn new(server: &McpServerConfig) -> Self {
        match server {
            McpServerConfig::Stdio(stdio) => Self::Stdio {
                command: stdio.command.clone(),
                args: stdio.args.clone(),
                cwd: stdio
                    .cwd
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                inherit_env: stdio.inherit_env.iter().map(ToString::to_string).collect(),
            },
            McpServerConfig::Http(http) => Self::Http {
                url: http.url.to_string(),
                auth: match http.auth {
                    McpHttpAuth::None => "none",
                    McpHttpAuth::Oauth => "oauth",
                },
                allow_insecure_loopback: http.allow_insecure_loopback,
            },
        }
    }
}
