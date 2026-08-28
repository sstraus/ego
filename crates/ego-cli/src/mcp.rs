//! Process-wide MCP configuration management.
//!
//! This module composes core's resolved configuration policy with the OS
//! adapter for the core-owned user-config transaction. It owns no protocol
//! connection, admission snapshot, health probe, or OAuth behavior.

use std::sync::Arc;

use ego_core::config::{
    McpConfigProvenance, McpHttpAuth, McpServerConfig, McpServerName, ResolvedMcpEntry, Source,
    UserConfigMutation, UserConfigPublication, UserConfigStore, UserMcpSlot,
};
use ego_core::run::RunTermination;
use ego_tools::config::OsUserConfigStore;
use ego_tools::store::OsIo;
use serde::Serialize;

use crate::cli::{McpAction, McpArgs, UsageError};
use crate::config::{self, McpManagementConfig};
use crate::output::Stdout;

/// Execute the configuration-only MCP management surface.
pub fn execute(args: &McpArgs, out: &Stdout) -> RunTermination {
    match execute_inner(args, out) {
        Ok(()) => RunTermination::Done,
        Err(termination) => termination,
    }
}

fn execute_inner(args: &McpArgs, out: &Stdout) -> Result<(), RunTermination> {
    match &args.action {
        McpAction::Add { name, server } => add(name, server, out),
        McpAction::Remove { name } => remove(name, out),
        McpAction::List { json } => list(*json, out),
    }
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
) -> Result<UserConfigPublication, RunTermination> {
    let store = OsUserConfigStore::new(Arc::new(OsIo), config.user_path.as_path());
    store
        .mutate(mutation)
        .map_err(|error| RunTermination::Failed {
            message: format!("user MCP configuration was not published: {error}"),
        })
}

fn add(name: &McpServerName, server: &McpServerConfig, out: &Stdout) -> Result<(), RunTermination> {
    let before = management_config()?;
    let slot = before.selected_user_slot();
    let publication = publish(
        &before,
        UserConfigMutation::McpSet {
            slot: slot.clone(),
            name: name.clone(),
            expected_user_entry: before.user_entry(&slot, name).cloned(),
            server: server.clone(),
        },
    )?;
    let after = resolve_after_publication(&publication)?;
    out.line(&format!(
        "{}; set MCP server `{name}`; {}; admission not observed",
        publication_text(&publication),
        effective_text(&after, name)
    ));
    Ok(())
}

#[derive(Clone, Copy)]
enum Removal {
    Entry,
    Tombstone,
    SystemMask,
}

fn remove(name: &McpServerName, out: &Stdout) -> Result<(), RunTermination> {
    let before = management_config()?;
    let (mutation, removal) = removal_mutation(&before, name)?;
    let publication = publish(&before, mutation)?;
    let after = resolve_after_publication(&publication)?;
    let consequence = match removal {
        Removal::Entry => format!("removed user entry; {}", effective_text(&after, name)),
        Removal::Tombstone => format!("removed user tombstone; {}", reveal_text(&after, name)),
        Removal::SystemMask => format!("masked system entry; {}", effective_text(&after, name)),
    };
    out.line(&format!(
        "{}; {consequence}; admission not observed",
        publication_text(&publication)
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
        } => delete_user_entry(config, UserMcpSlot::Base, name, removal),
        McpConfigProvenance::Profile {
            source: Source::User,
            name: profile,
        } => delete_user_entry(config, UserMcpSlot::Profile(profile.clone()), name, removal),
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
            let expected_user_entry = config.user_entry(&UserMcpSlot::Base, name).cloned();
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
    slot: UserMcpSlot,
    name: &McpServerName,
    removal: Removal,
) -> Result<(UserConfigMutation, Removal), RunTermination> {
    let expected_entry =
        config
            .user_entry(&slot, name)
            .cloned()
            .ok_or_else(|| RunTermination::Failed {
                message: format!(
                    "resolved user MCP server `{name}` is absent from its provenance slot"
                ),
            })?;
    Ok((
        UserConfigMutation::McpDeleteUserEntry {
            slot,
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

fn list(json: bool, out: &Stdout) -> Result<(), RunTermination> {
    let config = management_config()?;
    let listing = Listing::from_config(&config);
    if json {
        let rendered = serde_json::to_string(&listing).map_err(|error| RunTermination::Failed {
            message: format!("the MCP configuration listing could not be rendered: {error}"),
        })?;
        out.line(&rendered);
    } else if listing.servers.is_empty() {
        out.line("no MCP servers are configured; admission not observed");
    } else {
        for server in listing.servers {
            let state = if server.configured {
                "configured"
            } else {
                "masked"
            };
            out.line(&format!(
                "{}\t{state}\t{}\tadmission not observed",
                server.name,
                server.provenance.text()
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
    fn from_config(config: &McpManagementConfig) -> Self {
        let servers = config
            .effective
            .mcp
            .entries()
            .iter()
            .map(|(name, entry)| ServerReport::new(name, entry))
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
    provenance: ProvenanceReport,
    configuration: Option<ConfigurationReport>,
}

impl ServerReport {
    fn new(name: &McpServerName, entry: &ResolvedMcpEntry) -> Self {
        match entry {
            ResolvedMcpEntry::Configured { server, provenance } => Self {
                name: name.to_string(),
                configured: true,
                admitted: false,
                health: "not_observed",
                provenance: ProvenanceReport::new(provenance),
                configuration: Some(ConfigurationReport::new(server)),
            },
            ResolvedMcpEntry::Tombstone { provenance } => Self {
                name: name.to_string(),
                configured: false,
                admitted: false,
                health: "not_observed",
                provenance: ProvenanceReport::new(provenance),
                configuration: None,
            },
        }
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
