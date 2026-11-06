//! Human-owned, staged configuration. No setup probe, provider call, or run effect.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use ego_core::config::{
    ConfigLayer, ManagedConfigKey, ManagedConfigValue, ProfileName, Source, Trust,
    UserConfigMutation, UserConfigStore, UserConfigTarget, resolve_selected,
};
use ego_tools::config::OsUserConfigStore;
use ego_tools::store::OsIo;

use crate::config;
use crate::onboarding::ProviderReadiness;

/// Terminal and interactive-session seats share the same configuration flow.
/// Only the human seat supplies answers; model output never reaches this port.
pub(crate) trait Screen {
    fn choose(&mut self, title: &str, options: &[String]) -> Option<usize>;
    fn input(&mut self, prompt: &str) -> Option<String>;
    fn confirm(&mut self, preview: &str) -> bool;
    fn notice(&mut self, text: &str);
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
        Key::PermissionsJudge => effective.judge.as_ref().map(|model| model.0.clone()),
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
    if live && effective_value(key, effective) != effective_value(key, saved) {
        return "active session override".into();
    }
    if effective_value(key, effective).is_none() {
        return "default".into();
    }
    match key {
        Key::Mode => return effective.mode_provenance.to_string(),
        Key::Sandbox => return effective.sandbox_provenance.to_string(),
        Key::Roots => {
            return effective
                .roots_source
                .map_or_else(|| "default".into(), |source| source.to_string());
        }
        Key::SessionDir => {
            return effective
                .session_dir
                .as_ref()
                .map_or_else(|| "default".into(), |path| path.source.to_string());
        }
        Key::ArtifactDir => {
            return effective
                .artifact_dir
                .as_ref()
                .map_or_else(|| "default".into(), |path| path.source.to_string());
        }
        Key::Bare if effective.posture == ego_core::config::Posture::Bare => {
            return "untrusted workspace or bare setting".into();
        }
        _ => {}
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

fn profiles(workspace: &Path, home: &Path) -> Result<Vec<String>, String> {
    let layers = config::base_layers_in(workspace, Trust::Untrusted, Some(home))
        .map_err(|error| error.to_string())?;
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
) -> Result<Option<UserConfigTarget>, String> {
    if let Some(profile) = preset {
        return ProfileName::parse(profile)
            .map(UserConfigTarget::Profile)
            .map(Some)
            .map_err(|error| error.to_string());
    }
    let profiles = profiles(workspace, home)?;
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
) -> Result<
    (
        ego_core::config::Effective,
        Vec<ego_core::config::Sourced<ConfigLayer>>,
    ),
    String,
> {
    let layers = config::base_layers_in(workspace, Trust::Untrusted, Some(home))
        .map_err(|error| error.to_string())?;
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
) -> Result<
    (
        ego_core::config::Effective,
        Vec<ego_core::config::Sourced<ConfigLayer>>,
    ),
    String,
> {
    let mut layers = config::base_layers_in(workspace, Trust::Untrusted, Some(home))
        .map_err(|error| error.to_string())?;
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
    let Some(target) = target(screen, workspace, home, preset)? else {
        return Ok(());
    };
    let store = OsUserConfigStore::new(Arc::new(OsIo), home.join("config.toml"));
    let snapshot = store
        .snapshot(target.clone())
        .map_err(|error| error.to_string())?;
    let target_name = match &target {
        UserConfigTarget::Base => "User defaults".to_owned(),
        UserConfigTarget::Profile(name) => format!("Profile {name}"),
    };
    let (current, current_layers) = effective(workspace, home, &target)?;
    let keys = snapshot
        .values
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let mut staged: BTreeMap<ManagedConfigKey, Option<ManagedConfigValue>> = BTreeMap::new();
    loop {
        let mut choices = keys
            .iter()
            .map(|key| {
                let value = staged.get(key).map_or_else(
                    || display(key, selected_value(&snapshot.values, key)),
                    |value| display(key, value.as_ref()),
                );
                format!(
                    "{key} = {value} — {}{}",
                    describe(key),
                    if staged.contains_key(key) { " *" } else { "" }
                )
            })
            .collect::<Vec<_>>();
        for (key, description) in ADVANCED {
            choices.push(format!("{key} — {description} [advanced: config edit]"));
        }
        choices.push(format!(
            "providers — {} of {} ready [view credential setup]",
            providers.iter().filter(|provider| provider.ready).count(),
            providers.len()
        ));
        choices.push(format!("Review and save ({} pending)", staged.len()));
        choices.push("Discard and exit".into());
        let Some(index) = screen.choose(
            &format!("Setup · Settings (↑↓ Enter Esc) · {target_name}"),
            &choices,
        ) else {
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
            let (after, after_layers) =
                match candidate_effective(workspace, home, &target, batch.candidate()) {
                    Ok(after) => after,
                    Err(error) => {
                        screen.notice(&format!("Cannot save these settings: {error}"));
                        continue;
                    }
                };
            let before: toml::Value = toml::from_str(batch.before())
                .map_err(|error| format!("Cannot read setup before-values: {error}"))?;
            let before_target = target.profile().map_or(Some(&before), |profile| {
                before
                    .get("profile")
                    .and_then(|profiles| profiles.get(profile.as_str()))
            });
            let mut summary = format!(
                "Review: {}\nNo changes have been saved.\n",
                home.join("config.toml").display()
            );
            for (key, value) in &staged {
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
                summary.push_str(&format!(
                    "{key}: {before} -> {}\n",
                    display(key, value.as_ref())
                ));
                if let Some(resolved) = effective_value(key, &after) {
                    summary.push_str(&format!(
                        "  Effective for {target_name} after save: {resolved} ({})\n",
                        source_for(key, &after, &after_layers, &target, false, &after)
                    ));
                }
            }
            summary.push_str("Saved changes apply to new sessions; this session keeps its current permissions.\n");
            if !screen.confirm(&summary) {
                screen.notice("Not saved. Your edits are still staged.");
                continue;
            }
            match store.publish_batch(batch) {
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
        if index == keys.len() + ADVANCED.len() {
            let lines = providers
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            screen.choose(
                &format!("Providers — credential readiness\n{lines}\nCredentials come from environment variables or 'ego auth login'. Setup does not store them."),
                &["Back".into()],
            );
            continue;
        }
        if index >= keys.len() {
            let (key, description) = ADVANCED[index - keys.len()];
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
        let key = &keys[index];
        let stored = staged.get(key).map_or_else(
            || display(key, selected_value(&snapshot.values, key)),
            |value| display(key, value.as_ref()),
        );
        let shown_effective = current_session.unwrap_or(&current);
        let current_value = effective_value(key, shown_effective).unwrap_or_else(|| "unset".into());
        let options = vec![
            "Change value".to_owned(),
            "Unset (inherit the lower layer)".to_owned(),
            "Back".to_owned(),
        ];
        let title = format!(
            "{key} — {}\nStored: {stored}; effective: {current_value} ({})\nChanges to the saved target do not change this session.",
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
                if let Some(value) = edit_value(screen, key) {
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
            _ => {}
        }
    }
}

pub(crate) struct TerminalScreen;

struct RawMode;

impl RawMode {
    fn enter() -> Result<Self, String> {
        crossterm::terminal::enable_raw_mode().map_err(|error| error.to_string())?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

impl Screen for TerminalScreen {
    fn choose(&mut self, title: &str, options: &[String]) -> Option<usize> {
        use crossterm::{
            cursor,
            event::{self, Event, KeyCode, KeyEventKind},
            execute, terminal,
        };
        use std::io::{Write, stdout};
        if options.is_empty() {
            return None;
        }
        let _raw = RawMode::enter().ok()?;
        let mut selected = 0usize;
        loop {
            let mut out = stdout().lock();
            let (_, height) = terminal::size().unwrap_or((80, 24));
            let visible = height
                .saturating_sub(title.lines().count() as u16 + 3)
                .max(1) as usize;
            let start = selected.saturating_sub(visible - 1);
            if execute!(
                out,
                cursor::MoveTo(0, 0),
                terminal::Clear(terminal::ClearType::All)
            )
            .is_err()
            {
                return None;
            }
            for line in title.lines() {
                if writeln!(out, "{}\r", ego_core::permit::terminal_safe(line)).is_err() {
                    return None;
                }
            }
            if writeln!(out).is_err() {
                return None;
            }
            for (index, option) in options.iter().enumerate().skip(start).take(visible) {
                if writeln!(
                    out,
                    "{} {}\r",
                    if index == selected { '>' } else { ' ' },
                    ego_core::permit::terminal_safe(option)
                )
                .is_err()
                {
                    return None;
                }
            }
            if write!(
                out,
                "\r\n↑↓ move · Enter choose · Esc cancel · {}/{}\r\n",
                selected + 1,
                options.len()
            )
            .and_then(|_| out.flush())
            .is_err()
            {
                return None;
            }
            drop(out);
            match event::read().ok()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Up => selected = selected.checked_sub(1).unwrap_or(options.len() - 1),
                    KeyCode::Down => selected = (selected + 1) % options.len(),
                    KeyCode::PageUp => selected = selected.saturating_sub(visible),
                    KeyCode::PageDown => selected = (selected + visible).min(options.len() - 1),
                    KeyCode::Home => selected = 0,
                    KeyCode::End => selected = options.len() - 1,
                    KeyCode::Enter => return Some(selected),
                    KeyCode::Esc => return None,
                    _ => {}
                },
                _ => {}
            }
        }
    }

    fn input(&mut self, prompt: &str) -> Option<String> {
        use std::io::{Write, stdin, stdout};
        print!("\r\n{}: ", ego_core::permit::terminal_safe(prompt));
        stdout().flush().ok()?;
        let mut input = String::new();
        if stdin().read_line(&mut input).ok()? == 0 {
            return None;
        }
        Some(input.trim().to_owned())
    }

    fn confirm(&mut self, preview: &str) -> bool {
        self.choose(
            preview,
            &["No — keep editing".into(), "Yes — save changes".into()],
        ) == Some(1)
    }

    fn notice(&mut self, text: &str) {
        println!("{}", ego_core::permit::terminal_safe(text));
    }
}

#[cfg(test)]
mod tests {
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
        let original = "exec_paths=['/bin']\n[ui]\nvim=true\n[ui.statusline]\ntemplate='short'\n[profile.ci]\nsession_dir='/tmp/ego-ci'\n[auth.demo]\ncredential_command='echo sentinel-credential'\n";
        std::fs::write(&config, original).unwrap();
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
            base.details[0].contains("/bin") && base.details[0].contains("user"),
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
            effective_value(&ManagedConfigKey::PermissionsJudge, &resolved),
            None
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
    // row exists while the owner still has to edit the file.
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
    // old sessions are still compressed after the owner turned it off.
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
            let mut session = effective(&workspace, &home, &UserConfigTarget::Base)
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

    // Catches: a concurrent edit before review is overwritten under a stale before-value.
    #[test]
    fn setup_review_names_the_current_value_before_overwriting_a_concurrent_edit() {
        struct ReviewScreen {
            choices: VecDeque<&'static str>,
            config: std::path::PathBuf,
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
                assert!(
                    preview.contains("mode: \"auto\" -> \"default\""),
                    "review conceals the value being overwritten: {preview}"
                );
                true
            }
            fn notice(&mut self, _text: &str) {}
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
            ]
            .into(),
            config,
        };
        run(&mut screen, &workspace, &home, None, None, &[]).unwrap();
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
        let (current, layers) = effective(&workspace, &home, &UserConfigTarget::Base).unwrap();
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
