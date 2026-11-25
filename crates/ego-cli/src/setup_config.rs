//! Human-owned, staged configuration. No setup probe, provider call, or run effect.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use ego_core::config::{
    ConfigLayer, ManagedConfigKey, ManagedConfigValue, ProfileName, Source, Trust,
    UserConfigMutation, UserConfigStore, UserConfigTarget, resolve_selected,
};
use ego_core::permit::{RootAccess, RootCfg};
use ego_tools::config::OsUserConfigStore;
use ego_tools::store::OsIo;

use crate::config;
use crate::onboarding::ProviderReadiness;

/// Reads the configuration layers a fresh resolution would see. Production
/// reads the system, user and environment layers; project files are never read.
pub(crate) type Layers<'a> =
    &'a dyn Fn(&Path, &Path) -> Result<Vec<ego_core::config::Sourced<ConfigLayer>>, String>;

fn os_layers(
    workspace: &Path,
    home: &Path,
) -> Result<Vec<ego_core::config::Sourced<ConfigLayer>>, String> {
    config::base_layers_in(workspace, Trust::Untrusted, Some(home))
        .map_err(|error| error.to_string())
}

/// Terminal and interactive-session seats share the same configuration flow.
/// Only the human seat supplies answers; model output never reaches this port.
pub(crate) trait Screen {
    fn choose(&mut self, title: &str, options: &[String]) -> Option<usize>;
    fn input(&mut self, prompt: &str) -> Option<String>;
    fn confirm(&mut self, preview: &str) -> bool;
    fn notice(&mut self, text: &str);
    /// The seat cannot answer any more; setup ends without saving.
    fn lost(&self) -> bool {
        false
    }
}

fn describe(key: &ManagedConfigKey) -> &'static str {
    use ManagedConfigKey as Key;
    match key {
        Key::Model => "Select the default model for new runs.",
        Key::Thinking => "Set how much reasoning the model uses.",
        Key::Mode => "Choose when ego asks before a tool acts.",
        Key::Sandbox => "Limit which files tools and child processes can reach.",
        Key::Roots => "Allow access to extra directories outside the workspace.",
        Key::ExecPaths => "Allow executables in these extra directories.",
        Key::ExecEnv => "Choose which environment variables child processes receive.",
        Key::ExecEnvAllowlist => "Name variables that children may receive in allowlist mode.",
        Key::Network => "Allow or block network access during a run.",
        Key::WebPrivateNetwork => {
            "Let web_fetch reach every local and LAN address (true turns the block off)."
        }
        Key::Budget => "Limit the tokens or money a run can spend.",
        Key::Bare => "Ignore ambient project instructions and configuration.",
        Key::PermissionsJudge => "Choose a model to review uncertain tool requests.",
        Key::StatuslineTemplate => "Control the text shown above the prompt.",
        Key::Keybindings => "Change the keys used by the interactive editor.",
        Key::Vim => "Use Vim keys to edit the interactive prompt.",
        Key::CompactionIdle => "Compact an idle session shortly before its prompt cache expires.",
        Key::CompactionIdleMarginSeconds => {
            "Start idle compaction this many seconds before the cache expires."
        }
        Key::CompactionIdleThreshold => {
            "Skip idle compaction below this percent of the context window."
        }
        Key::Checkpoints => {
            "Save a private Git checkpoint before every file write so it can be restored."
        }
        Key::SessionsCompress => "Compress old sessions in the background (true is the default).",
        Key::Tools => {
            "Limit which built-in tools every session offers (an empty list offers none)."
        }
        Key::Cache => "Choose how long a provider may retain prompt cache entries.",
        Key::InstructionsMaxFiles => "Limit how many instruction files one run may load.",
        Key::CompactionHeadless => {
            "Compact a headless run that fills its context window and continue in the successor."
        }
        Key::DoctorNetwork => "Let ego doctor probe the network reachability of each provider.",
        Key::RecapEnabled => "Show a short recap at the first prompt after an absence.",
        Key::RecapAfterMinutes => {
            "Show the recap after this many minutes without a completed turn."
        }
        Key::McpTasks => {
            "Let HTTP MCP servers run long tools as tasks, and report each result when it ends."
        }
        Key::Memory => "Keep a memory file for this workspace that the model can add to.",
        Key::Lsp => {
            "Let the model ask language servers for definitions, references and diagnostics."
        }
        Key::BuildRecipes => {
            "Let the model save recipes that shorten repeated build output in this workspace."
        }
        Key::SessionDir => "Choose where CI sessions are saved.",
        Key::ArtifactDir => "Choose where CI artifacts are saved.",
        Key::AuthHeaders(_) => "Set identity headers for this provider.",
        Key::AuthCredentialCommand(_) => "Set the command that supplies provider credentials.",
    }
}

/// Valid file keys outside the typed owner API remain discoverable, not fake toggles.
const ADVANCED: &[(&str, &str)] = &[
    ("max_output_tokens", "Limit the length of each model reply."),
    (
        "compaction.threshold",
        "Compact when the context reaches this percent of the window.",
    ),
    ("task.model", "Choose the default model for task children."),
    (
        "task.effort",
        "Choose the default reasoning effort for task children.",
    ),
    (
        "permissions.allow",
        "Allow tool calls that match these exact rules.",
    ),
    (
        "permissions.deny",
        "Reject tool calls that match these rules.",
    ),
    (
        "instructions.shared_root",
        "Share instructions across repositories under one directory.",
    ),
    (
        "instructions.exclude_roots",
        "Exclude subtrees from shared instructions.",
    ),
    (
        "auth.PROVIDER.protocol_filter",
        "Route provider requests through a local protocol filter.",
    ),
    (
        "hooks",
        "Run declared handlers at matching lifecycle events.",
    ),
    (
        "mcp.servers",
        "Connect to configured external tool servers.",
    ),
    (
        "ui.statusline.providers",
        "Add command-provided values to the status line.",
    ),
];

fn display(key: &ManagedConfigKey, value: Option<&ManagedConfigValue>) -> String {
    match value {
        None => "unset".into(),
        Some(_) if key.is_secret() => "<redacted>".into(),
        Some(value) => value.to_toml(),
    }
}

fn category(key: &ManagedConfigKey) -> &'static str {
    use ManagedConfigKey as Key;
    match key {
        Key::Model | Key::Thinking | Key::Cache | Key::Budget => "Model and spending",
        Key::Mode
        | Key::Sandbox
        | Key::Roots
        | Key::ExecPaths
        | Key::Network
        | Key::WebPrivateNetwork
        | Key::ExecEnv
        | Key::ExecEnvAllowlist
        | Key::PermissionsJudge
        | Key::Bare
        | Key::Tools
        | Key::AuthHeaders(_)
        | Key::AuthCredentialCommand(_) => "Access and security",
        Key::StatuslineTemplate
        | Key::Keybindings
        | Key::Vim
        | Key::RecapEnabled
        | Key::RecapAfterMinutes => "Interface",
        Key::CompactionIdle
        | Key::CompactionIdleMarginSeconds
        | Key::CompactionIdleThreshold
        | Key::CompactionHeadless
        | Key::SessionsCompress
        | Key::Checkpoints => "Session and storage",
        _ => "Tools and integrations",
    }
}

fn preview(key: &ManagedConfigKey, value: Option<&ManagedConfigValue>) -> String {
    match value {
        Some(ManagedConfigValue::Roots(roots)) => format!("{} saved roots", roots.len()),
        Some(ManagedConfigValue::ExecPaths(paths)) => format!("{} paths", paths.len()),
        Some(ManagedConfigValue::Tools(tools)) => format!("{} tools", tools.len()),
        Some(ManagedConfigValue::ExecEnvAllowlist(names)) => format!("{} names", names.len()),
        _ => {
            let shown = display(key, value);
            if shown.chars().count() > 64 {
                format!("{}…", shown.chars().take(63).collect::<String>())
            } else {
                shown
            }
        }
    }
}

fn valid_root_path(path: &str) -> bool {
    !path.is_empty()
        && !path.chars().any(char::is_control)
        && (Path::new(path).is_absolute() || path.starts_with("~/"))
}

fn root_rows(roots: &[RootCfg]) -> Vec<String> {
    roots
        .iter()
        .map(|root| format!("{} — {}", root.access.as_str(), root.path.display()))
        .collect()
}

/// Edit one selected root at a time. The owner still reviews and publishes the
/// complete typed value through setup's existing atomic batch.
fn edit_roots(
    screen: &mut impl Screen,
    stored: Option<&ManagedConfigValue>,
) -> Option<ManagedConfigValue> {
    let mut roots = match stored {
        Some(ManagedConfigValue::Roots(roots)) => roots.clone(),
        _ => Vec::new(),
    };
    loop {
        let mut options = root_rows(&roots);
        options.extend(["Add root…".into(), "Back".into()]);
        let index = screen.choose(
            "Saved roots · Select one to edit (no change until review)",
            &options,
        )?;
        if index == roots.len() + 1 {
            return None;
        }
        if index == roots.len() {
            let Some(path) = screen.input("New root path (absolute or ~/path)") else {
                continue;
            };
            if !valid_root_path(&path) {
                screen.notice("Enter an absolute path or ~/path without control characters.");
                continue;
            }
            let access = match screen.choose(
                "Root access",
                &["Read only".into(), "Read and write".into(), "Back".into()],
            ) {
                Some(0) => RootAccess::Read,
                Some(1) => RootAccess::ReadWrite,
                _ => continue,
            };
            if roots.iter().any(|root| root.path == Path::new(&path)) {
                screen.notice("That root is already saved; select it to change its access.");
                continue;
            }
            roots.push(RootCfg {
                path: path.into(),
                access,
                consent: None,
            });
        } else {
            let choice = screen.choose(
                &format!(
                    "{} — {}",
                    roots[index].access.as_str(),
                    roots[index].path.display()
                ),
                &[
                    "Change path…".into(),
                    "Set read only".into(),
                    "Set read and write".into(),
                    "Remove this root".into(),
                    "Back".into(),
                ],
            );
            match choice {
                Some(0) => {
                    let Some(path) = screen.input("Replacement root path (absolute or ~/path)")
                    else {
                        continue;
                    };
                    if !valid_root_path(&path)
                        || roots
                            .iter()
                            .enumerate()
                            .any(|(other, root)| other != index && root.path == Path::new(&path))
                    {
                        screen.notice(
                            "Enter a unique absolute path or ~/path without control characters.",
                        );
                        continue;
                    }
                    roots[index].path = path.into();
                    roots[index].consent = None;
                }
                Some(1) => {
                    if roots[index].access != RootAccess::Read {
                        roots[index].access = RootAccess::Read;
                        roots[index].consent = None;
                    }
                }
                Some(2) => {
                    if roots[index].access != RootAccess::ReadWrite {
                        roots[index].access = RootAccess::ReadWrite;
                        roots[index].consent = None;
                    }
                }
                Some(3) => {
                    roots.remove(index);
                }
                _ => continue,
            }
        }
        let value = ManagedConfigValue::Roots(roots.clone());
        if ManagedConfigValue::parse(ManagedConfigKey::Roots, &value.to_toml()).is_err() {
            screen.notice("Invalid root value; no change was staged.");
            return None;
        }
        return Some(value);
    }
}

fn selected_value<'a>(
    values: &'a [(ManagedConfigKey, Option<ManagedConfigValue>)],
    key: &ManagedConfigKey,
) -> Option<&'a ManagedConfigValue> {
    values
        .iter()
        .find(|(candidate, _)| candidate == key)
        .and_then(|(_, value)| value.as_ref())
}

/// Report the effective value only when the existing resolver exposes it.
fn effective_value(
    key: &ManagedConfigKey,
    effective: &ego_core::config::Effective,
) -> Option<String> {
    use ManagedConfigKey as Key;
    match key {
        Key::Model => effective.model.as_ref().map(|value| value.0.clone()),
        Key::Thinking => effective.thinking.map(|value| value.as_str().into()),
        Key::Mode => Some(
            effective
                .mode
                .map_or("default", |mode| mode.as_str())
                .into(),
        ),
        Key::Sandbox => Some(effective.sandbox.unwrap_or_default().to_string()),
        Key::Roots => Some(ManagedConfigValue::Roots(effective.roots.clone()).to_toml()),
        Key::ExecPaths => {
            Some(ManagedConfigValue::ExecPaths(effective.exec_paths.clone()).to_toml())
        }
        Key::Network => Some(ManagedConfigValue::Network(effective.network).to_toml()),
        Key::WebPrivateNetwork => Some(effective.permissions.web_private_network.to_string()),
        Key::Budget => Some(format!(
            "money: {}; tokens: {}",
            effective.ceilings.money.map_or_else(
                || "none".into(),
                |money| format!("{}{}", money.amount, money.currency)
            ),
            effective
                .ceilings
                .tokens
                .map_or_else(|| "none".into(), |tokens| tokens.to_string())
        )),
        // An unset judge runs on the main model: say so, never show nothing.
        Key::PermissionsJudge => effective
            .judge
            .as_ref()
            .map(|model| model.0.clone())
            .or_else(|| {
                effective
                    .model
                    .as_ref()
                    .map(|model| format!("{} (the main model)", model.0))
            }),
        Key::Bare => Some((effective.posture == ego_core::config::Posture::Bare).to_string()),
        Key::ExecEnv => {
            Some(format!("{:?}", effective.exec.env.unwrap_or_default()).to_lowercase())
        }
        Key::ExecEnvAllowlist => Some(
            ManagedConfigValue::ExecEnvAllowlist(
                effective.exec.env_allowlist.clone().unwrap_or_default(),
            )
            .to_toml(),
        ),
        Key::StatuslineTemplate => effective
            .ui
            .statusline
            .as_ref()
            .and_then(|line| line.template.as_ref())
            .map(|value| ManagedConfigValue::StatuslineTemplate(value.clone()).to_toml()),
        Key::Keybindings => effective
            .ui
            .keybindings
            .as_ref()
            .map(|value| ManagedConfigValue::Keybindings(value.clone()).to_toml()),
        Key::Vim => Some(effective.ui.vim.unwrap_or(false).to_string()),
        Key::CompactionIdle => Some(effective.compaction.idle.to_string()),
        Key::CompactionHeadless => Some(effective.compaction.headless.to_string()),
        Key::DoctorNetwork => Some(effective.doctor_network.to_string()),
        Key::RecapEnabled => Some(effective.recap.enabled.to_string()),
        Key::RecapAfterMinutes => Some(effective.recap.after_minutes.to_string()),
        Key::McpTasks => Some(effective.mcp_tasks.to_string()),
        Key::Memory => Some(effective.memory.to_string()),
        Key::CompactionIdleMarginSeconds => {
            Some(effective.compaction.idle_margin_seconds.to_string())
        }
        Key::CompactionIdleThreshold => Some(effective.compaction.idle_threshold.get().to_string()),
        Key::Cache => Some(ManagedConfigValue::Cache(effective.cache).to_toml()),
        Key::Checkpoints => Some(effective.checkpoints.to_string()),
        Key::SessionsCompress => Some((effective.sessions.compress != Some(false)).to_string()),
        Key::Tools => effective
            .tools
            .as_ref()
            .map(|names| ManagedConfigValue::Tools(names.clone()).to_toml()),
        Key::InstructionsMaxFiles => Some(effective.instructions.limits().max_files.to_string()),
        Key::Lsp => Some(effective.lsp.to_string()),
        Key::BuildRecipes => Some(effective.build_recipes.to_string()),
        Key::SessionDir => effective
            .session_dir
            .as_ref()
            .map(|path| ManagedConfigValue::Path(path.value.clone()).to_toml()),
        Key::ArtifactDir => effective
            .artifact_dir
            .as_ref()
            .map(|path| ManagedConfigValue::Path(path.value.clone()).to_toml()),
        Key::AuthHeaders(provider) => effective
            .auth
            .get(provider)
            .and_then(|auth| (!auth.headers.is_empty()).then(|| "<redacted>".into())),
        Key::AuthCredentialCommand(provider) => effective.auth.get(provider).and_then(|auth| {
            auth.credential_command
                .as_ref()
                .map(|_| "<redacted>".into())
        }),
    }
}

fn source_for(
    key: &ManagedConfigKey,
    effective: &ego_core::config::Effective,
    layers: &[ego_core::config::Sourced<ConfigLayer>],
    target: &UserConfigTarget,
    live: bool,
    saved: &ego_core::config::Effective,
) -> String {
    use ManagedConfigKey as Key;
    // The resolver records these origins where it decides the value; read
    // them from the shown resolution instead of reconstructing them.
    let recorded = match key {
        Key::Mode => Some(effective.mode_provenance.to_string()),
        Key::Sandbox => Some(effective.sandbox_provenance.to_string()),
        Key::Roots => Some(
            effective
                .roots_source
                .map_or_else(|| "default".into(), |source| source.to_string()),
        ),
        Key::SessionDir => Some(
            effective
                .session_dir
                .as_ref()
                .map_or_else(|| "default".into(), |path| path.source.to_string()),
        ),
        Key::ArtifactDir => Some(
            effective
                .artifact_dir
                .as_ref()
                .map_or_else(|| "default".into(), |path| path.source.to_string()),
        ),
        _ => None,
    };
    if let Some(recorded) = recorded {
        return recorded;
    }
    if live && key.is_secret() {
        // Redacted values compare equal whatever their origin.
        return "unknown: session values are redacted".into();
    }
    if live && effective_value(key, effective) != effective_value(key, saved) {
        return "active session override".into();
    }
    let walked = walked_source(key, saved, layers, target);
    if live {
        format!("same as a fresh resolution: {walked}")
    } else {
        walked
    }
}

/// Reconstruct the winning layer over the layers setup itself loaded. Used
/// only for a fresh resolution, never as the origin of a session value.
fn walked_source(
    key: &ManagedConfigKey,
    effective: &ego_core::config::Effective,
    layers: &[ego_core::config::Sourced<ConfigLayer>],
    target: &UserConfigTarget,
) -> String {
    use ManagedConfigKey as Key;
    if effective_value(key, effective).is_none()
        || (matches!(key, Key::PermissionsJudge) && effective.judge.is_none())
    {
        return "default".into();
    }
    if matches!(key, Key::Bare) && effective.posture == ego_core::config::Posture::Bare {
        return "untrusted workspace or bare setting".into();
    }
    let mut ordered = layers.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|layer| match layer.source {
        Source::Default => 0,
        Source::System => 1,
        Source::User => 2,
        Source::Project => 3,
        Source::Local => 4,
        Source::Env => 5,
        Source::AgentFile => 6,
        Source::Flags => 7,
    });
    ordered.retain(|layer| {
        effective.posture != ego_core::config::Posture::Bare
            || !matches!(layer.source, Source::Project | Source::Local)
    });
    if let Key::AuthHeaders(provider) = key {
        let mut origins = BTreeMap::new();
        for layer in &ordered {
            if let Some(auth) = layer.value.auth.get(provider) {
                for name in auth.headers.keys() {
                    origins.insert(name.to_ascii_lowercase(), layer.source);
                }
            }
        }
        let mut sources = Vec::new();
        for source in origins.values() {
            if !sources.contains(source) {
                sources.push(*source);
            }
        }
        return sources
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
    }
    if matches!(key, Key::Budget) {
        let (mut money, mut tokens) = (None, None);
        for layer in &ordered {
            let budget = target
                .profile()
                .and_then(|name| layer.value.profile.get(name.as_str()))
                .and_then(|profile| profile.budget.as_ref())
                .or(layer.value.budget.as_ref());
            match budget {
                Some(ego_core::ceiling::Budget::Money(_)) => money = Some(layer.source),
                Some(ego_core::ceiling::Budget::Tokens(_)) => tokens = Some(layer.source),
                None => {}
            }
        }
        if effective.clamped.money.is_some() {
            money = Some(Source::System);
        }
        if effective.clamped.tokens.is_some() {
            tokens = Some(Source::System);
        }
        return format!(
            "money: {}; tokens: {}",
            money.unwrap_or(Source::Default),
            tokens.unwrap_or(Source::Default)
        );
    }
    let mut winning = None;
    for layer in ordered {
        let base = &layer.value;
        let profile = target
            .profile()
            .and_then(|name| base.profile.get(name.as_str()));
        let set = match key {
            Key::Model => profile
                .and_then(|p| p.model.as_ref())
                .or(base.model.as_ref())
                .is_some(),
            Key::Thinking => profile.and_then(|p| p.thinking).or(base.thinking).is_some(),
            Key::ExecPaths => profile
                .and_then(|p| p.exec_paths.as_ref())
                .or(base.exec_paths.as_ref())
                .is_some(),
            Key::ExecEnv => profile
                .and_then(|p| p.exec.as_ref())
                .and_then(|e| e.env)
                .or(base.exec.as_ref().and_then(|e| e.env))
                .is_some(),
            Key::ExecEnvAllowlist => profile
                .and_then(|p| p.exec.as_ref())
                .and_then(|e| e.env_allowlist.as_ref())
                .or(base.exec.as_ref().and_then(|e| e.env_allowlist.as_ref()))
                .is_some(),
            Key::Network => profile.and_then(|p| p.network).or(base.network).is_some(),
            Key::Budget => profile
                .and_then(|p| p.budget.as_ref())
                .or(base.budget.as_ref())
                .is_some(),
            Key::Bare => base.bare.is_some(),
            Key::WebPrivateNetwork => base.web_private_network.is_some(),
            Key::Memory => base.memory.is_some(),
            Key::Cache => base.cache.is_some(),
            Key::Tools => base.tools.is_some(),
            Key::Checkpoints => base.checkpoints.is_some(),
            Key::SessionsCompress => base
                .sessions
                .as_ref()
                .and_then(|sessions| sessions.compress)
                .is_some(),
            Key::CompactionIdleMarginSeconds => base
                .compaction
                .and_then(|compaction| compaction.idle_margin_seconds)
                .is_some(),
            Key::CompactionIdleThreshold => base
                .compaction
                .and_then(|compaction| compaction.idle_threshold)
                .is_some(),
            Key::InstructionsMaxFiles => base
                .instructions
                .as_ref()
                .and_then(|instructions| instructions.max_files)
                .is_some(),
            Key::Lsp => base.lsp.is_some(),
            Key::BuildRecipes => base.build_recipes.is_some(),
            Key::McpTasks => base.mcp_tasks.is_some(),
            Key::PermissionsJudge => profile
                .and_then(|p| p.permissions.as_ref())
                .or(base.permissions.as_ref())
                .is_some(),
            Key::StatuslineTemplate => base
                .ui
                .as_ref()
                .and_then(|ui| ui.statusline.as_ref())
                .and_then(|line| line.template.as_ref())
                .is_some(),
            Key::Keybindings => base
                .ui
                .as_ref()
                .and_then(|ui| ui.keybindings.as_ref())
                .is_some(),
            Key::Vim => base.ui.as_ref().and_then(|ui| ui.vim).is_some(),
            Key::CompactionIdle => base
                .compaction
                .and_then(|compaction| compaction.idle)
                .is_some(),
            Key::CompactionHeadless => base
                .compaction
                .and_then(|compaction| compaction.headless)
                .is_some(),
            Key::DoctorNetwork => base.doctor_network.is_some(),
            Key::RecapEnabled => base.recap.and_then(|recap| recap.enabled).is_some(),
            Key::RecapAfterMinutes => base.recap.and_then(|recap| recap.after_minutes).is_some(),
            Key::AuthHeaders(provider) => base
                .auth
                .get(provider)
                .is_some_and(|auth| !auth.headers.is_empty()),
            Key::AuthCredentialCommand(provider) => base
                .auth
                .get(provider)
                .is_some_and(|auth| auth.credential_command.is_some()),
            Key::Mode | Key::Sandbox | Key::Roots | Key::SessionDir | Key::ArtifactDir => false,
        };
        if set {
            winning = Some(layer.source);
        }
    }
    winning.map_or_else(|| "default".into(), |source| source.to_string())
}

/// Inspect advanced settings without admitting another mutation surface.
fn advanced_stored(key: &str, layer: &ConfigLayer, target: &UserConfigTarget) -> Option<String> {
    let profile = target
        .profile()
        .and_then(|name| layer.profile.get(name.as_str()));
    let base = target.profile().is_none();
    match key {
        "max_output_tokens" => profile
            .and_then(|profile| profile.max_output_tokens)
            .or_else(|| base.then_some(layer.max_output_tokens).flatten())
            .map(|value| value.to_string()),
        "permissions.allow" | "permissions.deny" => profile
            .and_then(|profile| profile.permissions.as_ref())
            .or_else(|| base.then_some(layer.permissions.as_ref()).flatten())
            .map(|permissions| {
                format!(
                    "{:?}",
                    if key == "permissions.allow" {
                        &permissions.allow
                    } else {
                        &permissions.deny
                    }
                )
            }),
        "mcp.servers" => {
            let entries = profile
                .map(|profile| &profile.mcp.servers)
                .or_else(|| base.then_some(&layer.mcp.servers))?;
            (!entries.is_empty()).then(|| format!("{entries:?}"))
        }
        _ if !base => None,
        "cache" => layer.cache.map(|value| format!("{value:?}")),
        "instructions.shared_root" => layer
            .instructions
            .as_ref()
            .and_then(|instructions| instructions.shared_root.as_ref())
            .map(|value| format!("{value:?}")),
        "instructions.exclude_roots" => layer
            .instructions
            .as_ref()
            .map(|instructions| format!("{:?}", instructions.exclude_roots)),
        "hooks" => (!layer.hooks.is_empty()).then(|| format!("{:?}", layer.hooks)),
        "task.model" => layer
            .task
            .as_ref()
            .and_then(|task| task.model.as_ref())
            .map(|model| model.0.clone()),
        "task.effort" => layer
            .task
            .as_ref()
            .and_then(|task| task.effort)
            .map(|effort| effort.as_str().to_owned()),
        "ui.statusline.providers" => layer
            .ui
            .as_ref()
            .and_then(|ui| ui.statusline.as_ref())
            .and_then(|line| line.providers.as_ref())
            .map(|providers| format!("{providers:?}")),
        _ => None,
    }
}

fn advanced_details(
    key: &str,
    target: &UserConfigTarget,
    effective: &ego_core::config::Effective,
    layers: &[ego_core::config::Sourced<ConfigLayer>],
) -> (String, String, String) {
    let stored = layers
        .iter()
        .find(|layer| layer.source == Source::User)
        .and_then(|layer| advanced_stored(key, &layer.value, target))
        .unwrap_or_else(|| "unset".into());
    let mut declarations = Vec::new();
    for source in [
        Source::Default,
        Source::System,
        Source::User,
        Source::Project,
        Source::Local,
        Source::Env,
        Source::AgentFile,
        Source::Flags,
    ] {
        if effective.posture == ego_core::config::Posture::Bare
            && matches!(source, Source::Project | Source::Local)
        {
            continue;
        }
        for layer in layers.iter().filter(|layer| layer.source == source) {
            let value = advanced_stored(key, &layer.value, target)
                .or_else(|| advanced_stored(key, &layer.value, &UserConfigTarget::Base));
            if let Some(value) = value {
                declarations.push((source, value));
            }
        }
    }
    let source = declarations
        .last()
        .map_or_else(|| "default".into(), |(source, _)| source.to_string());
    let (value, source) = match key {
        "max_output_tokens" => {
            let source = layers
                .iter()
                .find(|layer| {
                    layer.source == Source::System
                        && layer.value.max_output_tokens.is_some()
                        && layer.value.max_output_tokens == effective.max_output_tokens
                })
                .map_or(source, |_| "system (ceiling)".into());
            (
                effective
                    .max_output_tokens
                    .map_or_else(|| "unset".into(), |value| value.to_string()),
                source,
            )
        }
        "cache" => (format!("{:?}", effective.cache), source),
        "instructions.shared_root" => (
            format!("{:?}", effective.instructions.shared_root),
            if effective.instructions.shared_root.is_some() {
                "user"
            } else {
                "default"
            }
            .into(),
        ),
        "instructions.exclude_roots" => (
            format!("{:?}", effective.instructions.exclude_roots),
            source,
        ),
        "permissions.allow" | "permissions.deny" => {
            let rules = if key == "permissions.allow" {
                &effective.permissions.allow
            } else {
                &effective.permissions.deny
            };
            let values = rules
                .iter()
                .map(|rule| {
                    format!(
                        "{} ({})",
                        rule.rule(),
                        rule.origins()
                            .iter()
                            .map(|source| source.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            (format!("[{values}]"), "per-rule sources above".into())
        }
        "task.model" => (
            effective
                .task
                .model
                .as_ref()
                .map_or_else(|| "unset".into(), |model| model.0.clone()),
            source,
        ),
        "task.effort" => (
            effective
                .task
                .effort
                .map_or_else(|| "unset".into(), |effort| effort.as_str().to_owned()),
            source,
        ),
        "mcp.servers" => (
            format!("{:?}", effective.mcp.entries()),
            "per-entry provenance above".into(),
        ),
        "ui.statusline.providers" => (
            format!(
                "{:?}",
                effective
                    .ui
                    .statusline
                    .as_ref()
                    .and_then(|line| line.providers.as_ref())
            ),
            source,
        ),
        "hooks" => {
            let catalogue =
                ego_core::hook::HookCatalogue::admit_typed(layers, &[], Trust::Untrusted);
            let value = catalogue
                .map_err(|error| error.to_string())
                .and_then(|catalogue| {
                    crate::hooks::render(&catalogue, false).map_err(|error| error.to_string())
                })
                .unwrap_or_else(|error| format!("unavailable: {error}"));
            (value, "per-handler authority above".into())
        }
        _ => unreachable!("advanced catalogue key"),
    };
    (stored, value, source)
}

fn profiles(workspace: &Path, home: &Path, layers: Layers) -> Result<Vec<String>, String> {
    let layers = layers(workspace, home)?;
    let mut names = layers
        .iter()
        .filter(|layer| layer.source == Source::User)
        .flat_map(|layer| layer.value.profile.keys().cloned())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    Ok(names)
}

fn target(
    screen: &mut impl Screen,
    workspace: &Path,
    home: &Path,
    preset: Option<&str>,
    layers: Layers,
) -> Result<Option<UserConfigTarget>, String> {
    if let Some(profile) = preset {
        return ProfileName::parse(profile)
            .map(UserConfigTarget::Profile)
            .map(Some)
            .map_err(|error| error.to_string());
    }
    let profiles = profiles(workspace, home, layers)?;
    let path = home.join("config.toml");
    let mut options = vec![format!("User defaults — {}", path.display())];
    options.extend(
        profiles
            .iter()
            .map(|name| format!("Profile {name} — {}", path.display())),
    );
    options.push("New profile…".into());
    let Some(index) = screen.choose("Setup · Choose where to save", &options) else {
        return Ok(None);
    };
    if index == 0 {
        return Ok(Some(UserConfigTarget::Base));
    }
    if index <= profiles.len() {
        return ProfileName::parse(&profiles[index - 1])
            .map(UserConfigTarget::Profile)
            .map(Some)
            .map_err(|error| error.to_string());
    }
    let Some(name) = screen.input("New profile name") else {
        return Ok(None);
    };
    ProfileName::parse(&name)
        .map(UserConfigTarget::Profile)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn effective(
    workspace: &Path,
    home: &Path,
    target: &UserConfigTarget,
    layers: Layers,
) -> Result<
    (
        ego_core::config::Effective,
        Vec<ego_core::config::Sourced<ConfigLayer>>,
    ),
    String,
> {
    let layers = layers(workspace, home)?;
    let active = target.profile().and_then(|profile| {
        layers
            .iter()
            .any(|layer| layer.value.profile.contains_key(profile.as_str()))
            .then_some((profile.as_str(), Source::Flags))
    });
    let resolved =
        resolve_selected(&layers, Trust::Untrusted, active).map_err(|error| error.to_string())?;
    Ok((resolved, layers))
}

fn candidate_effective(
    workspace: &Path,
    home: &Path,
    target: &UserConfigTarget,
    candidate: &str,
    layers: Layers,
) -> Result<
    (
        ego_core::config::Effective,
        Vec<ego_core::config::Sourced<ConfigLayer>>,
    ),
    String,
> {
    let mut layers = layers(workspace, home)?;
    let user = ConfigLayer::parse(Source::User, candidate).map_err(|error| error.to_string())?;
    if let Some(layer) = layers.iter_mut().find(|layer| layer.source == Source::User) {
        *layer = user;
    } else {
        let position = layers
            .iter()
            .position(|layer| layer.source != Source::System)
            .unwrap_or(layers.len());
        layers.insert(position, user);
    }
    let active = target
        .profile()
        .map(|profile| (profile.as_str(), Source::Flags));
    let resolved =
        resolve_selected(&layers, Trust::Untrusted, active).map_err(|error| error.to_string())?;
    Ok((resolved, layers))
}

fn edit_value(screen: &mut impl Screen, key: &ManagedConfigKey) -> Option<ManagedConfigValue> {
    use ManagedConfigKey as Key;
    let choices: Option<&[&str]> = match key {
        Key::Mode => Some(&["plan", "default", "auto", "accept-edits", "yolo"]),
        Key::Sandbox => Some(&["ro", "workspace", "adaptive", "off"]),
        Key::Thinking => Some(&["off", "minimal", "low", "medium", "high", "xhigh", "max"]),
        Key::Network => Some(&["on", "off"]),
        Key::ExecEnv => Some(&["inherit", "core", "allowlist"]),
        Key::Cache => Some(&["none", "short", "long"]),
        Key::Bare
        | Key::Vim
        | Key::CompactionIdle
        | Key::CompactionHeadless
        | Key::DoctorNetwork
        | Key::RecapEnabled
        | Key::Memory
        | Key::Checkpoints
        | Key::SessionsCompress
        | Key::Lsp
        | Key::BuildRecipes
        | Key::McpTasks
        | Key::WebPrivateNetwork => Some(&["true", "false"]),
        _ => None,
    };
    let raw = if let Some(choices) = choices {
        let rows = choices
            .iter()
            .map(|choice| (*choice).to_owned())
            .collect::<Vec<_>>();
        let selected = screen.choose(&format!("Choose {key}"), &rows)?;
        format!("\"{}\"", choices[selected])
    } else {
        screen.input(&format!("{key}: enter one TOML value"))?
    };
    let raw = if matches!(
        key,
        Key::Bare
            | Key::Vim
            | Key::CompactionIdle
            | Key::CompactionHeadless
            | Key::DoctorNetwork
            | Key::RecapEnabled
            | Key::Memory
            | Key::Checkpoints
            | Key::SessionsCompress
            | Key::Lsp
            | Key::BuildRecipes
            | Key::McpTasks
            | Key::WebPrivateNetwork
    ) {
        raw.trim_matches('"').to_owned()
    } else {
        raw
    };
    match ManagedConfigValue::parse(key.clone(), &raw) {
        Ok(value) => Some(value),
        Err(error) => {
            screen.notice(&format!("Invalid {key}: {error}"));
            None
        }
    }
}

/// Keys whose saved value changes what a new session may read, run, reach or spend.
fn security_sensitive(key: &ManagedConfigKey) -> bool {
    use ManagedConfigKey as Key;
    matches!(
        key,
        Key::Mode
            | Key::Sandbox
            | Key::Roots
            | Key::ExecPaths
            | Key::ExecEnv
            | Key::ExecEnvAllowlist
            | Key::Network
            | Key::WebPrivateNetwork
            | Key::Budget
            | Key::PermissionsJudge
            | Key::Tools
            | Key::Bare
            | Key::AuthHeaders(_)
            | Key::AuthCredentialCommand(_)
    )
}

/// Where a review resolves its effective values.
#[derive(Clone, Copy)]
struct ReviewScope<'a> {
    target: &'a UserConfigTarget,
    target_name: &'a str,
    workspace: &'a Path,
    home: &'a Path,
    layers: Layers<'a>,
    live: bool,
}

/// The exact text a human confirms. Publication recomputes it under the
/// store lock and refuses when any reviewed input changed.
fn review(
    staged: &BTreeMap<ManagedConfigKey, Option<ManagedConfigValue>>,
    before: &str,
    candidate: &str,
    scope: ReviewScope<'_>,
) -> Result<String, String> {
    let ReviewScope {
        target,
        target_name,
        workspace,
        home,
        layers,
        live,
    } = scope;
    let (after, after_layers) = candidate_effective(workspace, home, target, candidate, layers)?;
    let before: toml::Value = toml::from_str(before)
        .map_err(|error| format!("Cannot read setup before-values: {error}"))?;
    let before_target = target.profile().map_or(Some(&before), |profile| {
        before
            .get("profile")
            .and_then(|profiles| profiles.get(profile.as_str()))
    });
    let mut summary = format!(
        "Review: {} · {target_name}\nNo changes have been saved.\n",
        home.join("config.toml").display()
    );
    for (key, value) in staged {
        let key_name = key.as_str();
        let path = match key {
            ManagedConfigKey::AuthHeaders(provider) => {
                vec!["auth", provider.as_str(), "headers"]
            }
            ManagedConfigKey::AuthCredentialCommand(provider) => {
                vec!["auth", provider.as_str(), "credential_command"]
            }
            _ => key_name.split('.').collect(),
        };
        let before_value = path.iter().fold(before_target, |value, part| {
            value.and_then(|value| value.get(*part))
        });
        let before = before_value.map_or_else(
            || "unset".into(),
            |value| {
                if key.is_secret() {
                    "<redacted>".into()
                } else {
                    value.to_string()
                }
            },
        );
        if let ManagedConfigKey::Roots = key {
            summary.push_str("roots: saved entries before change:\n");
            if let Some(entries) = before_value.and_then(toml::Value::as_array) {
                for entry in entries {
                    if let (Some(path), Some(access)) = (
                        entry.get("path").and_then(toml::Value::as_str),
                        entry.get("access").and_then(toml::Value::as_str),
                    ) {
                        summary.push_str(&format!("  {access} — {path}\n"));
                    }
                }
                if entries.is_empty() {
                    summary.push_str("  (none)\n");
                }
            } else {
                summary.push_str("  unset\n");
            }
            summary.push_str("roots: saved entries after change:\n");
            match value {
                Some(ManagedConfigValue::Roots(roots)) if !roots.is_empty() => {
                    for row in root_rows(roots) {
                        summary.push_str(&format!("  {row}\n"));
                    }
                }
                Some(ManagedConfigValue::Roots(_)) => {
                    summary.push_str("  (none; workspace only)\n")
                }
                None => summary.push_str("  unset (inherit the lower layer)\n"),
                _ => unreachable!("roots key holds typed roots"),
            }
            summary.push_str(&format!(
                "  Effective after save: {} roots ({})\n",
                after.roots.len(),
                source_for(key, &after, &after_layers, target, false, &after)
            ));
        } else {
            summary.push_str(&format!(
                "{key}: {before} -> {}\n",
                display(key, value.as_ref())
            ));
            if let Some(resolved) = effective_value(key, &after) {
                summary.push_str(&format!("  Effective after save, fresh resolution of {target_name} without project files: {resolved} ({})\n", source_for(key, &after, &after_layers, target, false, &after)));
            }
        }
        if security_sensitive(key) {
            summary.push_str(
                "  Security-sensitive: this changes what new sessions may do. A running session keeps the perimeter it captured.\n",
            );
        }
    }
    summary.push_str(if live {
        "This session keeps its captured settings and permissions. Quit and resume it without a prompt, or start a new session, to use saved changes; /config perimeter shows what this session holds.\n"
    } else {
        "Saved changes apply to sessions that start or resume after the save; a running session keeps the perimeter it captured.\n"
    });
    summary.push_str("Save these changes?");
    Ok(summary)
}

/// Stage changes until a human confirms one adapter-owned atomic candidate.
/// A declined preview, terminal loss, or a changed base publishes nothing.
pub(crate) fn run(
    screen: &mut impl Screen,
    workspace: &Path,
    home: &Path,
    preset: Option<&str>,
    current_session: Option<&ego_core::config::Effective>,
    providers: &[ProviderReadiness],
) -> Result<(), String> {
    run_with(
        screen,
        workspace,
        home,
        preset,
        current_session,
        providers,
        &os_layers,
    )
}

fn run_with(
    screen: &mut impl Screen,
    workspace: &Path,
    home: &Path,
    preset: Option<&str>,
    current_session: Option<&ego_core::config::Effective>,
    providers: &[ProviderReadiness],
    layers: Layers,
) -> Result<(), String> {
    let Some(target) = target(screen, workspace, home, preset, layers)? else {
        return Ok(());
    };
    let store = OsUserConfigStore::new(Arc::new(OsIo), home.join("config.toml"));
    // The inspected target stays pinned until publication: a different
    // projection at review reloads instead of becoming an unreviewed baseline.
    let mut snapshot = store
        .snapshot(target.clone())
        .map_err(|error| error.to_string())?;
    let target_name = match &target {
        UserConfigTarget::Base => "User defaults".to_owned(),
        UserConfigTarget::Profile(name) => format!("Profile {name}"),
    };
    let (mut current, mut current_layers) = effective(workspace, home, &target, layers)?;
    let keys = snapshot
        .values
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let mut staged: BTreeMap<ManagedConfigKey, Option<ManagedConfigValue>> = BTreeMap::new();
    let mut search = String::new();
    loop {
        let matches = |name: &str, description: &str| {
            search.is_empty()
                || name.to_lowercase().contains(&search)
                || description.to_lowercase().contains(&search)
        };
        let mut visible = keys
            .iter()
            .enumerate()
            .filter(|(_, key)| matches(key.as_str().as_ref(), describe(key)))
            .collect::<Vec<_>>();
        visible.sort_by_key(|(_, key)| category(key));
        let advanced = ADVANCED
            .iter()
            .enumerate()
            .filter(|(_, (key, description))| matches(key, description))
            .collect::<Vec<_>>();
        let mut choices = vec![if search.is_empty() {
            "Search settings…".into()
        } else {
            format!("Search settings… ({search})")
        }];
        if !search.is_empty() {
            choices.push("Clear search".into());
        }
        let first_setting = choices.len();
        choices.extend(visible.iter().map(|(_, key)| {
            let value = staged.get(*key).map_or_else(
                || preview(key, selected_value(&snapshot.values, key)),
                |value| preview(key, value.as_ref()),
            );
            format!(
                "{key} = {value} · {} — {}{}",
                category(key),
                describe(key),
                if staged.contains_key(*key) { " *" } else { "" }
            )
        }));
        for (_, (key, description)) in &advanced {
            choices.push(format!("{key} — {description} [advanced: config edit]"));
        }
        if matches("providers", "credential presence") {
            choices.push("providers — credential presence and setup help [presence only; diagnosis: ego doctor]".into());
        }
        let providers_index = (matches("providers", "credential presence"))
            .then_some(first_setting + visible.len() + advanced.len());
        choices.push(format!("Review and save ({} pending)", staged.len()));
        choices.push("Discard and exit".into());
        let Some(index) = screen.choose(&format!("Setup · Settings · {target_name}"), &choices)
        else {
            if screen.lost() {
                screen.notice("Setup lost its terminal; nothing was saved.");
                return Ok(());
            }
            if staged.is_empty() || screen.confirm("Discard unsaved changes?") {
                return Ok(());
            }
            continue;
        };
        if index == choices.len() - 1 {
            if staged.is_empty() || screen.confirm("Discard unsaved changes?") {
                return Ok(());
            }
            continue;
        }
        if index == choices.len() - 2 {
            if staged.is_empty() {
                screen.notice("No changes to save.");
                continue;
            }
            let fresh = store
                .snapshot(target.clone())
                .map_err(|error| error.to_string())?;
            if fresh != snapshot {
                snapshot = fresh;
                staged.retain(|key, value| selected_value(&snapshot.values, key) != value.as_ref());
                (current, current_layers) = effective(workspace, home, &target, layers)?;
                screen.notice(&format!(
                    "{} changed since setup read it. Setup reloaded the stored values; your staged edits are kept. Check the list, then review again.",
                    home.join("config.toml").display()
                ));
                continue;
            }
            let changes = staged
                .iter()
                .map(|(key, value)| match value {
                    Some(value) => UserConfigMutation::ManagedSet {
                        target: target.clone(),
                        key: key.clone(),
                        value: value.clone(),
                    },
                    None => UserConfigMutation::ManagedUnset {
                        target: target.clone(),
                        key: key.clone(),
                    },
                })
                .collect::<Vec<_>>();
            let batch = store
                .prepare_batch(changes)
                .map_err(|error| error.to_string())?;
            let live = current_session.is_some();
            let scope = ReviewScope {
                target: &target,
                target_name: &target_name,
                workspace,
                home,
                layers,
                live,
            };
            let reviewed = |batch: &dyn ego_core::config::UserConfigBatch| {
                review(&staged, batch.before(), batch.candidate(), scope)
            };
            let summary = match reviewed(&*batch) {
                Ok(summary) => summary,
                Err(error) => {
                    screen.notice(&format!("Cannot save these settings: {error}"));
                    continue;
                }
            };
            if !screen.confirm(&summary) {
                screen.notice("Not saved. Your edits are still staged.");
                continue;
            }
            let (before, candidate) = (batch.before().to_owned(), batch.candidate().to_owned());
            let recheck = || {
                let again = review(&staged, &before, &candidate, scope)
                    .map_err(ego_core::config::UserConfigError::new)?;
                if again == summary {
                    Ok(())
                } else {
                    Err(ego_core::config::UserConfigError::new(
                        "configuration inputs changed after setup preview; review a fresh preview",
                    ))
                }
            };
            match store.publish_batch_checked(batch, &recheck) {
                Ok(result) => {
                    screen.notice(&format!(
                        "Saved: {:?}. Start or resume a session to use saved perimeter changes.",
                        result.publication
                    ));
                    return Ok(());
                }
                Err(error) => {
                    screen.notice(&format!(
                        "Not saved: {error}. Review the current configuration before retrying."
                    ));
                    return Ok(());
                }
            }
        }
        if index == 0 {
            if let Some(query) = screen.input("Search settings by key or description") {
                search = query.trim().to_lowercase();
            }
            continue;
        }
        if !search.is_empty() && index == 1 {
            search.clear();
            continue;
        }
        if Some(index) == providers_index {
            let lines = providers
                .iter()
                .map(|row| {
                    let state = if row.ready {
                        "credential source present"
                    } else {
                        "no credential source found"
                    };
                    match &row.note {
                        Some(note) => format!("{}: {state} — {note}", row.provider),
                        None => format!("{}: {state}", row.provider),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            screen.choose(
                &format!("Providers — credential presence (presence only)\n{lines}\nPresence means a key variable is set or a credential is stored. It does not prove that the credential works or that the provider answers. Set credentials with environment variables or 'ego auth login'; check them with 'ego auth status'; diagnose with 'ego doctor'. Setup does not store credentials and calls no provider."),
                &["Back".into()],
            );
            continue;
        }
        if index >= first_setting + visible.len() {
            let (_, (key, description)) = advanced[index - first_setting - visible.len()];
            let shown_effective = current_session.unwrap_or(&current);
            let (stored, value, source) =
                advanced_details(key, &target, shown_effective, &current_layers);
            let (_, saved_value, _) = advanced_details(key, &target, &current, &current_layers);
            let source = if current_session.is_some() && value != saved_value {
                "active session override".to_owned()
            } else {
                source
            };
            screen.choose(
                &format!("{key} — {description}\nStored: {stored}; effective: {value} ({source})\nRead-only. Use ego config edit to change this setting."),
                &["Back".into()],
            );
            continue;
        }
        let key = visible[index - first_setting].1;
        let staged_value = staged
            .get(key)
            .map_or_else(|| selected_value(&snapshot.values, key), Option::as_ref);
        let stored = match staged_value {
            Some(ManagedConfigValue::Roots(roots)) => {
                let rows = root_rows(roots);
                if rows.is_empty() {
                    "no saved roots".into()
                } else {
                    format!("\n{}", rows.join("\n"))
                }
            }
            _ => display(key, staged_value),
        };
        let shown_effective = current_session.unwrap_or(&current);
        let current_value = if matches!(key, ManagedConfigKey::Roots) {
            format!("{} active roots", shown_effective.roots.len())
        } else {
            effective_value(key, shown_effective).unwrap_or_else(|| "unset".into())
        };
        let mut options = vec![
            if matches!(key, ManagedConfigKey::Roots) {
                "Edit saved roots one by one".to_owned()
            } else {
                "Change value".to_owned()
            },
            "Unset (inherit the lower layer)".to_owned(),
        ];
        if matches!(key, ManagedConfigKey::Roots) {
            options.push("Inspect active roots (read only)".into());
        }
        options.push("Back".into());
        let context = if current_session.is_some() {
            "this session, captured at start"
        } else {
            "fresh resolution without project files"
        };
        let title = format!(
            "{key} — {}\nStored: {stored}; effective: {current_value} ({})\nEffective value: {context}. Changes to the saved target do not change a running session.",
            describe(key),
            source_for(
                key,
                shown_effective,
                &current_layers,
                &target,
                current_session.is_some(),
                &current
            )
        );
        match screen.choose(&title, &options) {
            Some(0) => {
                let edited = if matches!(key, ManagedConfigKey::Roots) {
                    edit_roots(screen, staged_value)
                } else {
                    edit_value(screen, key)
                };
                if let Some(value) = edited {
                    if selected_value(&snapshot.values, key) == Some(&value) {
                        staged.remove(key);
                    } else {
                        staged.insert(key.clone(), Some(value));
                    }
                }
            }
            Some(1) => {
                if selected_value(&snapshot.values, key).is_none() {
                    staged.remove(key);
                } else {
                    staged.insert(key.clone(), None);
                }
            }
            Some(2) if matches!(key, ManagedConfigKey::Roots) => {
                let mut rows = shown_effective
                    .roots
                    .iter()
                    .map(|root| format!("{} — {}", root.access.as_str(), root.path.display()))
                    .collect::<Vec<_>>();
                rows.push("Back".into());
                screen.choose("Active roots captured by this session (read only)", &rows);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    /// An absolute path spelling valid on the host: Windows needs a drive letter.
    fn abs(unix: &str) -> String {
        if cfg!(windows) {
            format!("C:{unix}")
        } else {
            unix.to_owned()
        }
    }

    use super::*;
    use std::collections::VecDeque;

    struct ScriptScreen {
        choices: VecDeque<&'static str>,
        config: std::path::PathBuf,
        concurrent_edit: bool,
        notices: Vec<String>,
    }

    impl Screen for ScriptScreen {
        fn choose(&mut self, _title: &str, options: &[String]) -> Option<usize> {
            let label = self.choices.pop_front()?;
            Some(
                options
                    .iter()
                    .position(|option| option.starts_with(label))
                    .expect(label),
            )
        }

        fn input(&mut self, _prompt: &str) -> Option<String> {
            None
        }

        fn confirm(&mut self, preview: &str) -> bool {
            assert!(preview.contains("mode:"));
            if self.concurrent_edit {
                std::fs::write(&self.config, "# concurrent edit\nmode='auto'\n").unwrap();
            }
            true
        }

        fn notice(&mut self, text: &str) {
            self.notices.push(text.to_owned());
        }
    }

    // Catches: compact previews hide saved/staged arrays behind a different effective value.
    #[test]
    fn setting_details_show_full_saved_and_staged_arrays_not_effective_substitutes() {
        struct InspectScreen {
            selections: VecDeque<&'static str>,
            details: Vec<String>,
        }
        impl Screen for InspectScreen {
            fn choose(&mut self, title: &str, options: &[String]) -> Option<usize> {
                if title.contains("Stored:") {
                    self.details.push(title.to_owned());
                }
                let label = self.selections.pop_front()?;
                Some(
                    options
                        .iter()
                        .position(|option| option.starts_with(label))
                        .expect(label),
                )
            }
            fn input(&mut self, _prompt: &str) -> Option<String> {
                Some(format!(
                    "['{}', '{}']",
                    abs("/staged/first"),
                    abs("/staged/second")
                ))
            }
            fn confirm(&mut self, _preview: &str) -> bool {
                true
            }
            fn notice(&mut self, _text: &str) {}
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let (saved_first, saved_second) = (abs("/saved/first"), abs("/saved/second"));
        let (staged_first, staged_second) = (abs("/staged/first"), abs("/staged/second"));
        let effective_only = abs("/effective/only");
        let original = format!("exec_paths=['{saved_first}', '{saved_second}']\n");
        std::fs::write(home.join("config.toml"), &original).unwrap();
        let layers = vec![
            ConfigLayer::parse(Source::Flags, &format!("exec_paths=['{effective_only}']\n"))
                .unwrap(),
        ];
        let active = resolve_selected(&layers, Trust::Untrusted, None).unwrap();
        let mut screen = InspectScreen {
            selections: [
                "User defaults",
                "exec_paths =",
                "Change value",
                "exec_paths =",
                "Back",
                "Discard and exit",
            ]
            .into(),
            details: Vec::new(),
        };
        run(&mut screen, &workspace, &home, None, Some(&active), &[]).unwrap();
        assert_eq!(screen.details.len(), 2);
        for (detail, first, second) in [
            (&screen.details[0], &saved_first, &saved_second),
            (&screen.details[1], &staged_first, &staged_second),
        ] {
            let (stored, effective) = detail.split_once("; effective:").unwrap();
            assert!(
                stored.contains(first) && stored.contains(second),
                "{detail}"
            );
            assert!(stored.find(first) < stored.find(second), "{detail}");
            assert!(!stored.contains(&effective_only), "{detail}");
            assert!(effective.contains(&effective_only), "{detail}");
        }
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            original
        );
    }

    // Catches: a managed setting displays a count or a config-ls hint instead of its resolved value and source.
    #[test]
    fn setting_details_show_resolved_values_and_sources() {
        struct InspectScreen {
            selections: VecDeque<&'static str>,
            details: Vec<String>,
            catalogue: Vec<String>,
        }
        impl Screen for InspectScreen {
            fn choose(&mut self, title: &str, options: &[String]) -> Option<usize> {
                if title.contains("Stored:") {
                    self.details.push(title.to_owned());
                }
                if title.contains("· Settings") {
                    self.catalogue.extend(options.iter().cloned());
                }
                let label = self.selections.pop_front()?;
                Some(
                    options
                        .iter()
                        .position(|option| option.starts_with(label))
                        .expect(label),
                )
            }
            fn input(&mut self, _prompt: &str) -> Option<String> {
                None
            }
            fn confirm(&mut self, _preview: &str) -> bool {
                false
            }
            fn notice(&mut self, _text: &str) {}
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let config = home.join("config.toml");
        let exec_root = abs("/bin");
        let original = format!(
            "exec_paths=['{exec_root}']\n[ui]\nvim=true\n[ui.statusline]\ntemplate='short'\n[profile.ci]\nsession_dir='/tmp/ego-ci'\n[auth.demo]\ncredential_command='echo sentinel-credential'\n"
        );
        std::fs::write(&config, &original).unwrap();
        let mut base = InspectScreen {
            selections: [
                "User defaults",
                "exec_paths =",
                "Back",
                "ui.statusline.template =",
                "Back",
                "auth.demo.headers =",
                "Back",
                "auth.demo.credential_command =",
                "Back",
                "Discard and exit",
            ]
            .into(),
            details: Vec::new(),
            catalogue: Vec::new(),
        };
        run(&mut base, &workspace, &home, None, None, &[]).unwrap();
        assert!(
            base.details[0].contains(&exec_root) && base.details[0].contains("user"),
            "{}",
            base.details[0]
        );
        assert!(
            base.details[1].contains("short") && base.details[1].contains("user"),
            "{}",
            base.details[1]
        );
        assert!(
            base.details[2].contains("effective: unset (default)"),
            "{}",
            base.details[2]
        );
        assert!(
            base.details[3].contains("effective: <redacted> (user)"),
            "{}",
            base.details[3]
        );
        assert!(
            base.details
                .iter()
                .all(|text| !text.contains("sentinel-credential")
                    && !text.contains("see config ls --effective"))
        );
        assert!(
            base.catalogue
                .iter()
                .any(|row| row.starts_with("auth.demo.headers"))
        );
        assert!(
            base.catalogue
                .iter()
                .any(|row| row.starts_with("auth.demo.credential_command"))
        );
        assert!(
            base.catalogue
                .iter()
                .all(|row| !row.contains("sentinel-credential"))
        );
        let mut ci = InspectScreen {
            selections: ["session_dir =", "Back", "Discard and exit"].into(),
            details: Vec::new(),
            catalogue: Vec::new(),
        };
        run(&mut ci, &workspace, &home, Some("ci"), None, &[]).unwrap();
        assert!(
            ci.details[0].contains("/tmp/ego-ci") && ci.details[0].contains("user"),
            "{}",
            ci.details[0]
        );
        assert_eq!(std::fs::read_to_string(config).unwrap(), original);
    }

    // Catches: filtering settings selects a different security key, or editing one root
    // replaces the whole list and loses the other saved root.
    #[test]
    fn setup_search_and_edit_one_root_preserves_other_entries() {
        struct Seat {
            choices: VecDeque<&'static str>,
            inputs: VecDeque<String>,
            menus: Vec<Vec<String>>,
        }
        impl Screen for Seat {
            fn choose(&mut self, _title: &str, options: &[String]) -> Option<usize> {
                self.menus.push(options.to_vec());
                let label = self.choices.pop_front()?;
                Some(
                    options
                        .iter()
                        .position(|option| option.contains(label))
                        .expect(label),
                )
            }
            fn input(&mut self, _prompt: &str) -> Option<String> {
                self.inputs.pop_front()
            }
            fn confirm(&mut self, preview: &str) -> bool {
                assert!(
                    preview.contains("roots: saved entries after change:"),
                    "{preview}"
                );
                assert!(
                    preview.contains("read-write —") && preview.contains("read —"),
                    "{preview}"
                );
                true
            }
            fn notice(&mut self, text: &str) {
                assert!(text.starts_with("Saved:"), "{text}");
            }
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        let alpha = root.path().join("external/alpha");
        let beta = root.path().join("external/beta");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let config = home.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "mode='plan'\nroots=[{{path='{}',access='read'}},{{path='{}',access='read'}}]\n",
                alpha.display(),
                beta.display()
            ),
        )
        .unwrap();
        let mut seat = Seat {
            choices: [
                "User defaults",
                "Search settings",
                "roots =",
                "Edit saved roots",
                "external/alpha",
                "Set read and write",
                "Review and save",
            ]
            .into(),
            inputs: ["roots".into()].into(),
            menus: Vec::new(),
        };
        run(&mut seat, &workspace, &home, None, None, &[]).unwrap();
        assert!(seat.choices.is_empty() && seat.inputs.is_empty());
        assert!(seat.menus.iter().any(|rows| {
            rows.iter()
                .any(|row| row.starts_with("roots = 2 saved roots"))
        }));
        let filtered = &seat.menus[2];
        assert!(filtered.iter().any(|row| row.starts_with("roots =")));
        assert!(!filtered.iter().any(|row| row.starts_with("mode =")));
        let stored = ConfigLayer::parse(Source::User, &std::fs::read_to_string(config).unwrap())
            .unwrap()
            .value;
        let roots = stored.roots.unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].path, alpha);
        assert_eq!(roots[0].access, RootAccess::ReadWrite);
        assert_eq!(roots[1].path, beta);
        assert_eq!(roots[1].access, RootAccess::Read);
        assert_eq!(stored.mode.unwrap().as_str(), "plan");
    }

    // Catches: cancelling a root editor or clearing a query stages or saves a
    // perimeter change without the human confirming it.
    #[test]
    fn setup_cancelled_root_editor_and_search_leave_config_unchanged() {
        struct Seat(VecDeque<&'static str>, VecDeque<String>);
        impl Screen for Seat {
            fn choose(&mut self, _title: &str, options: &[String]) -> Option<usize> {
                let label = self.0.pop_front()?;
                Some(
                    options
                        .iter()
                        .position(|option| option.starts_with(label))
                        .expect(label),
                )
            }
            fn input(&mut self, _prompt: &str) -> Option<String> {
                self.1.pop_front()
            }
            fn confirm(&mut self, _preview: &str) -> bool {
                panic!("no change needs consent")
            }
            fn notice(&mut self, text: &str) {
                assert_eq!(text, "No changes to save.");
            }
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let original = "mode='plan'\nroots=[]\n";
        std::fs::write(home.join("config.toml"), original).unwrap();
        let mut seat = Seat(
            [
                "User defaults",
                "roots =",
                "Edit saved roots",
                "Add root",
                "Back",
                "Back",
                "Search settings",
                "Clear search",
                "Review and save",
                "Discard and exit",
            ]
            .into(),
            [abs("/tmp/not-published"), abs("nonexistent")].into(),
        );
        run(&mut seat, &workspace, &home, None, None, &[]).unwrap();
        assert!(seat.0.is_empty());
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            original
        );
    }

    // Catches: removing one saved root clears the entire roots field, or a
    // system root is presented as a root the user can edit in an empty target.
    #[test]
    fn setup_removes_only_selected_root_and_keeps_inherited_roots_read_only() {
        struct Seat {
            choices: VecDeque<&'static str>,
            titles: Vec<String>,
        }
        impl Screen for Seat {
            fn choose(&mut self, title: &str, options: &[String]) -> Option<usize> {
                self.titles.push(title.into());
                let choice = self.choices.pop_front()?;
                Some(
                    options
                        .iter()
                        .position(|option| option.contains(choice))
                        .expect(choice),
                )
            }
            fn input(&mut self, _prompt: &str) -> Option<String> {
                None
            }
            fn confirm(&mut self, preview: &str) -> bool {
                assert!(
                    preview.contains("roots: saved entries before change:"),
                    "{preview}"
                );
                assert!(
                    preview.contains("roots: saved entries after change:"),
                    "{preview}"
                );
                true
            }
            fn notice(&mut self, text: &str) {
                assert!(text.starts_with("Saved:"), "{text}");
            }
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        let first = root.path().join("first");
        let second = root.path().join("second");
        for path in [&home, &workspace, &first, &second] {
            std::fs::create_dir_all(path).unwrap();
        }
        let config = home.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "roots=[{{path='{}',access='read'}},{{path='{}',access='read'}}]\n",
                first.display(),
                second.display()
            ),
        )
        .unwrap();
        let mut seat = Seat {
            choices: [
                "User defaults",
                "roots =",
                "Edit saved roots",
                "first",
                "Remove this root",
                "Review and save",
            ]
            .into(),
            titles: Vec::new(),
        };
        run(&mut seat, &workspace, &home, None, None, &[]).unwrap();
        let stored = ConfigLayer::parse(Source::User, &std::fs::read_to_string(config).unwrap())
            .unwrap()
            .value
            .roots
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].path, second);
        assert!(seat.titles.iter().any(|title| {
            title.contains(&format!(
                "Stored: \nread — {}\nread — {}; effective: 2 active roots",
                first.display(),
                second.display()
            ))
        }));
        let other = tempfile::tempdir().unwrap();
        let inherited_home = other.path().join("home");
        std::fs::create_dir_all(&inherited_home).unwrap();
        let system = ConfigLayer::parse(
            Source::System,
            &format!("roots=[{{path='{}',access='read'}}]", first.display()),
        )
        .unwrap();
        let layers = |_: &Path, _: &Path| Ok(vec![system.clone()]);
        let mut inherited = Seat {
            choices: [
                "User defaults",
                "roots =",
                "Inspect active roots",
                "Back",
                "Discard and exit",
            ]
            .into(),
            titles: Vec::new(),
        };
        run_with(
            &mut inherited,
            &workspace,
            &inherited_home,
            None,
            None,
            &[],
            &layers,
        )
        .unwrap();
        assert!(
            inherited
                .titles
                .iter()
                .any(|title| title.contains("Stored: unset; effective: 1 active roots"))
        );
        assert!(inherited.choices.is_empty());
        assert!(!inherited_home.join("config.toml").exists());
    }

    // Catches: source labels follow input order instead of resolver precedence or misattribute an unset judge.
    #[test]
    fn source_labels_follow_resolved_precedence() {
        let layers = vec![
            ConfigLayer::parse(Source::Env, "model='env-model'\n").unwrap(),
            ConfigLayer::parse(
                Source::User,
                "model='user-model'\n[permissions]\njudge='reviewer'\n",
            )
            .unwrap(),
            ConfigLayer::parse(Source::Flags, "[permissions]\nallow=[]\n").unwrap(),
        ];
        let resolved = resolve_selected(&layers, Trust::Untrusted, None).unwrap();
        let base = UserConfigTarget::Base;
        assert_eq!(
            effective_value(&ManagedConfigKey::Model, &resolved).as_deref(),
            Some("env-model")
        );
        assert_eq!(
            source_for(
                &ManagedConfigKey::Model,
                &resolved,
                &layers,
                &base,
                false,
                &resolved
            ),
            "env"
        );
        assert_eq!(
            effective_value(&ManagedConfigKey::PermissionsJudge, &resolved).as_deref(),
            Some("env-model (the main model)")
        );
        assert_eq!(
            source_for(
                &ManagedConfigKey::PermissionsJudge,
                &resolved,
                &layers,
                &base,
                false,
                &resolved
            ),
            "default"
        );
    }

    // Catches: the owner cannot turn the web private-address block off from
    // setup, or setup stores a value the resolver does not hand to the policy.
    #[test]
    fn setup_turns_the_web_private_address_block_off_in_user_config() {
        struct Choices(VecDeque<&'static str>);
        impl Screen for Choices {
            fn choose(&mut self, _title: &str, options: &[String]) -> Option<usize> {
                let label = self.0.pop_front()?;
                Some(
                    options
                        .iter()
                        .position(|option| option.starts_with(label))
                        .expect(label),
                )
            }
            fn input(&mut self, _prompt: &str) -> Option<String> {
                None
            }
            fn confirm(&mut self, preview: &str) -> bool {
                assert!(preview.contains("web_private_network"), "{preview}");
                true
            }
            fn notice(&mut self, _text: &str) {}
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let mut screen = Choices(
            [
                "User defaults",
                "web_private_network =",
                "Change value",
                "true",
                "Review and save",
            ]
            .into(),
        );
        run(&mut screen, &workspace, &home, None, None, &[]).unwrap();
        assert!(screen.0.is_empty());
        let text = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let layers = [ConfigLayer::parse(Source::User, &text).unwrap()];
        let resolved = resolve_selected(&layers, Trust::Untrusted, None).unwrap();
        assert!(resolved.permissions.web_private_network, "{text}");
    }

    // Catches: a scalar key that `ego setup` only names is shown but not written, so the
    // row exists while the owner still has to edit the file (story 884-78dd).
    #[test]
    fn setup_writes_the_typed_scalar_rows_that_were_advanced() {
        struct Script {
            choices: VecDeque<&'static str>,
            inputs: VecDeque<&'static str>,
        }
        impl Screen for Script {
            fn choose(&mut self, _title: &str, options: &[String]) -> Option<usize> {
                let label = self.choices.pop_front()?;
                Some(
                    options
                        .iter()
                        .position(|option| option.starts_with(label))
                        .expect(label),
                )
            }
            fn input(&mut self, _prompt: &str) -> Option<String> {
                self.inputs.pop_front().map(str::to_owned)
            }
            fn confirm(&mut self, _preview: &str) -> bool {
                true
            }
            fn notice(&mut self, text: &str) {
                assert!(text.starts_with("Saved:"), "unexpected notice: {text}");
            }
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let mut screen = Script {
            choices: [
                "User defaults",
                "cache =",
                "Change value",
                "long",
                "compaction.idle_margin_seconds =",
                "Change value",
                "compaction.idle_threshold =",
                "Change value",
                "instructions.max_files =",
                "Change value",
                "Review and save",
            ]
            .into(),
            inputs: ["45", "40", "7"].into(),
        };
        run(&mut screen, &workspace, &home, None, None, &[]).unwrap();
        assert!(screen.choices.is_empty() && screen.inputs.is_empty());
        let text = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let layers = [ConfigLayer::parse(Source::User, &text).unwrap()];
        let resolved = resolve_selected(&layers, Trust::Untrusted, None).unwrap();
        assert_eq!(
            resolved.cache,
            ego_core::generator::capabilities::CacheRetention::Long,
            "{text}"
        );
        assert_eq!(resolved.compaction.idle_margin_seconds.get(), 45, "{text}");
        assert_eq!(resolved.compaction.idle_threshold.get(), 40, "{text}");
        assert_eq!(resolved.instructions.limits().max_files, 7, "{text}");
    }

    // Catches: the compression row is shown but `ego setup` writes nothing the job reads, so
    // old sessions are still compressed after the owner turned it off (story 882-c71b).
    #[test]
    fn setup_turns_cold_session_compression_off_in_user_config() {
        struct Choices(VecDeque<&'static str>);
        impl Screen for Choices {
            fn choose(&mut self, _title: &str, options: &[String]) -> Option<usize> {
                let label = self.0.pop_front()?;
                Some(
                    options
                        .iter()
                        .position(|option| option.starts_with(label))
                        .expect(label),
                )
            }
            fn input(&mut self, _prompt: &str) -> Option<String> {
                None
            }
            fn confirm(&mut self, _preview: &str) -> bool {
                true
            }
            fn notice(&mut self, text: &str) {
                assert!(text.starts_with("Saved:"), "unexpected notice: {text}");
            }
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let mut screen = Choices(
            [
                "User defaults",
                "sessions.compress =",
                "Change value",
                "false",
                "Review and save",
            ]
            .into(),
        );
        run(&mut screen, &workspace, &home, None, None, &[]).unwrap();
        assert!(screen.0.is_empty());
        let text = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let layers = [ConfigLayer::parse(Source::User, &text).unwrap()];
        let resolved = resolve_selected(&layers, Trust::Untrusted, None).unwrap();
        assert_eq!(resolved.sessions.compress_after(), None, "{text}");
    }

    // Catches: setup overwrites another writer's bytes after showing a stale preview.
    #[test]
    fn setup_refuses_a_candidate_when_config_changes_during_confirmation() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let config = home.join("config.toml");
        std::fs::write(&config, "mode='plan'\n").unwrap();
        let mut screen = ScriptScreen {
            choices: [
                "User defaults",
                "mode =",
                "Change value",
                "default",
                "Review and save",
            ]
            .into(),
            config: config.clone(),
            concurrent_edit: true,
            notices: Vec::new(),
        };
        run(&mut screen, &workspace, &home, None, None, &[]).unwrap();
        assert_eq!(
            std::fs::read_to_string(config).unwrap(),
            "# concurrent edit\nmode='auto'\n"
        );
        assert!(
            screen
                .notices
                .iter()
                .any(|text| text.contains("changed after setup preview"))
        );
        assert!(screen.choices.is_empty());
    }

    // Catches: a configured advanced setting has no stored/effective value or source in setup.
    #[test]
    fn setup_advanced_setting_exposes_its_stored_and_effective_values() {
        struct InspectAdvanced {
            choices: VecDeque<&'static str>,
            details: Vec<String>,
        }
        impl Screen for InspectAdvanced {
            fn choose(&mut self, title: &str, options: &[String]) -> Option<usize> {
                if title.contains("Stored:") {
                    self.details.push(title.to_owned());
                }
                let label = self.choices.pop_front()?;
                options.iter().position(|option| option.starts_with(label))
            }
            fn input(&mut self, _prompt: &str) -> Option<String> {
                None
            }
            fn confirm(&mut self, _preview: &str) -> bool {
                false
            }
            fn notice(&mut self, _text: &str) {}
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(home.join("config.toml"), "max_output_tokens=123\n").unwrap();
        let mut screen = InspectAdvanced {
            choices: [
                "User defaults",
                "max_output_tokens",
                "Back",
                "Discard and exit",
            ]
            .into(),
            details: Vec::new(),
        };
        run(&mut screen, &workspace, &home, None, None, &[]).unwrap();
        assert!(
            screen.details.iter().any(|detail| detail.contains("123")
                && detail.contains("effective:")
                && detail.contains("user")),
            "setup never exposes the configured advanced value: {:?}",
            screen.details
        );
    }

    // Catches: read-only advanced details substitute the user base for a selected profile or live override.
    #[test]
    fn advanced_details_preserve_profile_inheritance_and_live_overrides() {
        struct Inspect {
            choices: VecDeque<&'static str>,
            details: Vec<String>,
        }
        impl Screen for Inspect {
            fn choose(&mut self, title: &str, options: &[String]) -> Option<usize> {
                if title.contains("Stored:") {
                    self.details.push(title.to_owned());
                }
                let label = self.choices.pop_front()?;
                options.iter().position(|option| option.starts_with(label))
            }
            fn input(&mut self, _: &str) -> Option<String> {
                panic!("read-only setting requested input")
            }
            fn confirm(&mut self, _: &str) -> bool {
                panic!("read-only setting requested publication")
            }
            fn notice(&mut self, _: &str) {}
        }
        for (profile, live, expected) in [
            (Some("docs"), false, "Stored: 45; effective: 45 (user)"),
            (
                Some("inherited"),
                false,
                "Stored: unset; effective: 123 (user)",
            ),
            (
                None,
                true,
                "Stored: 123; effective: 9 (active session override)",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().join("home");
            let workspace = root.path().join("workspace");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&workspace).unwrap();
            let config = home.join("config.toml");
            let original = "max_output_tokens=123\n[profile.docs]\nmax_output_tokens=45\n[profile.inherited]\nmode='plan'\n";
            std::fs::write(&config, original).unwrap();
            let mut session = effective(&workspace, &home, &UserConfigTarget::Base, &os_layers)
                .unwrap()
                .0;
            session.max_output_tokens = std::num::NonZeroU64::new(9);
            let mut choices = VecDeque::new();
            if profile.is_none() {
                choices.push_back("User defaults");
            }
            choices.extend(["max_output_tokens", "Back", "Discard and exit"]);
            let mut screen = Inspect {
                choices,
                details: Vec::new(),
            };
            run(
                &mut screen,
                &workspace,
                &home,
                profile,
                live.then_some(&session),
                &[],
            )
            .unwrap();
            assert!(
                screen.details.iter().any(|title| title.contains(expected)),
                "{expected}: {:?}",
                screen.details
            );
            assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
            assert!(screen.choices.is_empty());
        }
    }

    // Catches: an edit made after setup read the target silently becomes the
    // reviewed baseline, or is overwritten under a stale before-value.
    #[test]
    fn setup_reloads_when_the_target_changed_since_inspection() {
        struct ReviewScreen {
            choices: VecDeque<&'static str>,
            config: std::path::PathBuf,
            notices: Vec<String>,
            previews: Vec<String>,
        }
        impl Screen for ReviewScreen {
            fn choose(&mut self, _title: &str, options: &[String]) -> Option<usize> {
                let label = self.choices.pop_front()?;
                if label == "Review and save" {
                    std::fs::write(&self.config, "mode='auto'\n").unwrap();
                }
                options.iter().position(|option| option.starts_with(label))
            }
            fn input(&mut self, _prompt: &str) -> Option<String> {
                None
            }
            fn confirm(&mut self, preview: &str) -> bool {
                self.previews.push(preview.to_owned());
                true
            }
            fn notice(&mut self, text: &str) {
                self.notices.push(text.to_owned());
            }
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let config = home.join("config.toml");
        std::fs::write(&config, "mode='plan'\n").unwrap();
        let mut screen = ReviewScreen {
            choices: [
                "User defaults",
                "mode =",
                "Change value",
                "default",
                "Review and save",
                "Review and save",
            ]
            .into(),
            config: config.clone(),
            notices: Vec::new(),
            previews: Vec::new(),
        };
        run(&mut screen, &workspace, &home, None, None, &[]).unwrap();
        assert!(
            screen.notices[0].contains("changed since setup read it"),
            "{:?}",
            screen.notices
        );
        assert_eq!(
            screen.previews.len(),
            1,
            "first review must not open a save"
        );
        assert!(
            screen.previews[0].contains("mode: \"auto\" -> \"default\""),
            "{}",
            screen.previews[0]
        );
        let text = std::fs::read_to_string(config).unwrap();
        assert!(text.contains("default"), "{text}");
    }

    fn workspace_home() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        (root, workspace, home)
    }

    struct Contract {
        choices: VecDeque<&'static str>,
        inputs: VecDeque<&'static str>,
        previews: Vec<String>,
        notices: Vec<String>,
        details: Vec<String>,
        on_confirm: Box<dyn FnMut()>,
    }

    impl Contract {
        fn new(choices: &[&'static str], inputs: &[&'static str]) -> Self {
            Self {
                choices: choices.iter().copied().collect(),
                inputs: inputs.iter().copied().collect(),
                previews: Vec::new(),
                notices: Vec::new(),
                details: Vec::new(),
                on_confirm: Box::new(|| {}),
            }
        }
    }

    impl Screen for Contract {
        fn choose(&mut self, title: &str, options: &[String]) -> Option<usize> {
            if title.contains("Stored:") || title.starts_with("Providers") {
                self.details.push(title.to_owned());
            }
            let label = self.choices.pop_front()?;
            Some(
                options
                    .iter()
                    .position(|option| option.starts_with(label))
                    .expect(label),
            )
        }
        fn input(&mut self, _prompt: &str) -> Option<String> {
            self.inputs.pop_front().map(str::to_owned)
        }
        fn confirm(&mut self, preview: &str) -> bool {
            self.previews.push(preview.to_owned());
            (self.on_confirm)();
            true
        }
        fn notice(&mut self, text: &str) {
            self.notices.push(text.to_owned());
        }
    }

    // Catches: the review shows the requested budget as effective although a
    // system ceiling binds it, or the save loosens the system floor.
    #[test]
    fn review_states_the_system_floor_that_binds_a_saved_value() {
        let (_root, workspace, home) = workspace_home();
        let layers = |workspace: &Path, home: &Path| {
            let mut layers = os_layers(workspace, home)?;
            layers.insert(
                0,
                ConfigLayer::parse(Source::System, "budget='1k'\n").unwrap(),
            );
            Ok(layers)
        };
        let mut screen = Contract::new(
            &[
                "User defaults",
                "budget =",
                "Change value",
                "Review and save",
            ],
            &["\"5k\""],
        );
        run_with(&mut screen, &workspace, &home, None, None, &[], &layers).unwrap();
        let preview = &screen.previews[0];
        assert!(preview.contains("tokens: 1000"), "{preview}");
        assert!(preview.contains("tokens: system"), "{preview}");
        assert!(preview.contains("Security-sensitive"), "{preview}");
        let text = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let resolved =
            resolve_selected(&layers(&workspace, &home).unwrap(), Trust::Untrusted, None).unwrap();
        assert_eq!(resolved.ceilings.tokens, Some(1000), "{text}");
    }

    // Catches: a higher layer that changes between preview and publication
    // makes the human confirm an effective result that is no longer true.
    #[test]
    fn publication_refuses_when_a_reviewed_input_changed_during_confirmation() {
        let (_root, workspace, home) = workspace_home();
        let config = home.join("config.toml");
        std::fs::write(&config, "mode='plan'\n").unwrap();
        let env = std::rc::Rc::new(std::cell::RefCell::new(None::<&'static str>));
        let seen = env.clone();
        let layers = move |workspace: &Path, home: &Path| {
            let mut layers = os_layers(workspace, home)?;
            if let Some(text) = *seen.borrow() {
                layers.push(ConfigLayer::parse(Source::Flags, text).unwrap());
            }
            Ok(layers)
        };
        let mut screen = Contract::new(
            &[
                "User defaults",
                "mode =",
                "Change value",
                "default",
                "Review and save",
            ],
            &[],
        );
        let changed = env.clone();
        screen.on_confirm = Box::new(move || *changed.borrow_mut() = Some("mode='auto'\n"));
        run_with(&mut screen, &workspace, &home, None, None, &[], &layers).unwrap();
        assert_eq!(std::fs::read_to_string(config).unwrap(), "mode='plan'\n");
        assert!(
            screen
                .notices
                .iter()
                .any(|text| text.contains("configuration inputs changed after setup preview")),
            "{:?}",
            screen.notices
        );
    }

    // Catches: setup tells a live session that a saved perimeter change takes
    // effect now, or attributes a redacted session value to a layer.
    #[test]
    fn a_live_session_keeps_its_captured_authority_after_a_save() {
        let (_root, workspace, home) = workspace_home();
        let config = home.join("config.toml");
        std::fs::write(
            &config,
            "sandbox='ro'\n[auth.demo]\ncredential_command='echo s'\n",
        )
        .unwrap();
        let session = effective(&workspace, &home, &UserConfigTarget::Base, &os_layers)
            .unwrap()
            .0;
        let mut screen = Contract::new(
            &[
                "User defaults",
                "auth.demo.credential_command =",
                "Back",
                "sandbox =",
                "Change value",
                "off",
                "Review and save",
            ],
            &[],
        );
        run(&mut screen, &workspace, &home, None, Some(&session), &[]).unwrap();
        assert!(
            screen.details[0].contains("unknown: session values are redacted"),
            "{}",
            screen.details[0]
        );
        assert!(screen.details[1].contains("this session, captured at start"));
        let preview = &screen.previews[0];
        assert!(preview.contains("Security-sensitive"), "{preview}");
        assert!(
            preview.contains("This session keeps its captured"),
            "{preview}"
        );
        assert!(std::fs::read_to_string(config).unwrap().contains("off"));
    }

    // Catches: setup presents credential presence as provider health with an
    // aggregate ready score instead of presence-only guidance.
    #[test]
    fn provider_help_is_presence_only_without_a_score() {
        let (_root, workspace, home) = workspace_home();
        let providers = [ProviderReadiness {
            provider: "demo".into(),
            ready: false,
            note: Some("set DEMO_API_KEY or run 'ego auth login demo'".into()),
        }];
        let mut catalogue = Contract::new(
            &["User defaults", "providers", "Back", "Discard and exit"],
            &[],
        );
        run(&mut catalogue, &workspace, &home, None, None, &providers).unwrap();
        let help = &catalogue.details[0];
        assert!(
            help.contains("presence only") && help.contains("ego doctor"),
            "{help}"
        );
        assert!(help.contains("DEMO_API_KEY"), "{help}");
        assert!(!help.contains("ready"), "{help}");
    }

    // Catches: editing a named profile accidentally changes the user base setting.
    #[test]
    fn setup_writes_only_the_selected_profile() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let config = home.join("config.toml");
        std::fs::write(&config, "mode='plan'\n").unwrap();
        let mut screen = ScriptScreen {
            choices: ["mode =", "Change value", "default", "Review and save"].into(),
            config: config.clone(),
            concurrent_edit: false,
            notices: Vec::new(),
        };
        run(&mut screen, &workspace, &home, Some("docs"), None, &[]).unwrap();
        let parsed = ego_core::config::ConfigLayer::parse(
            Source::User,
            &std::fs::read_to_string(config).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed.value.mode.map(|mode| mode.as_str()), Some("plan"));
        assert_eq!(
            parsed.value.profile["docs"].mode.map(|mode| mode.as_str()),
            Some("default")
        );
        assert!(screen.choices.is_empty());
    }

    #[test]
    fn task_default_rows_show_the_stored_and_effective_values() {
        // Catches: a `task.*` advanced row without a detail arm, which panics
        // `ego setup` when the owner selects it, or shows a stored key as unset.
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "[task]\nmodel='anthropic/claude-opus-5-5'\neffort='xhigh'\n",
        )
        .unwrap();
        let (current, layers) =
            effective(&workspace, &home, &UserConfigTarget::Base, &os_layers).unwrap();
        for (key, expected) in [
            ("task.model", "anthropic/claude-opus-5-5"),
            ("task.effort", "xhigh"),
        ] {
            let (stored, value, source) =
                advanced_details(key, &UserConfigTarget::Base, &current, &layers);
            assert!(
                stored.to_lowercase().contains(expected),
                "{key} stored: {stored}"
            );
            assert!(
                value.to_lowercase().contains(expected),
                "{key} value: {value}"
            );
            assert_eq!(source, "user", "{key}");
        }
    }
}
