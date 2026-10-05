//! Where the config layers of `CLI.md` §7 live, and how the command line
//! becomes one of them.
//!
//! Only the *locations* are here. Parsing is `ConfigLayer::parse` and
//! precedence is `config::resolve`, both pure and both in `ego-core`; this
//! module knows which files to look in and what `EGO_HOME` means, which is
//! process knowledge and belongs to the process.
//!
//! Invalid config is never skipped (§4.5): an unknown key is a hard error
//! from the layer it was written in, with the key and the source named.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ego_core::ceiling::Ceilings;
use ego_core::config::{
    AuthProviderLayer, ConfigError, ConfigLayer, Effective, ManagedConfigValue,
    McpConfigProvenance, McpEnvironmentName, McpHttpAuth, McpHttpConfig, McpHttpUrl, McpLayerEntry,
    McpServerConfig, McpServerName, ProfileLayer, ProfileName, ResolvedMcpEntry, Ruleset, Source,
    Sourced, Trust, UserConfigMutation, UserConfigMutationFacts, UserConfigMutationResult,
    UserConfigPublication, UserConfigSnapshot, UserConfigStore, UserConfigTarget, resolve,
    resolve_selected,
};
use ego_core::run::RunTermination;
use ego_core::turn::CancelToken;
use ego_tools::config::OsUserConfigStore;
use ego_tools::exec::run_editor_process;
use ego_tools::store::{OsIo, StoreIo};
use serde::Deserialize;

use crate::cli::{Args, ConfigAction, ConfigArgs, McpSelector, SessionMetadataArgs, UsageError};
use crate::output::{self, Diagnostics, Stdout, empty_listing};

/// The system layer: an administrator's fleet defaults and the only source of
/// non-relaxable ceilings.
const SYSTEM_CONFIG: &str = "/etc/ego/config.toml";
/// The directory [`SYSTEM_CONFIG`] lives in.
///
/// Shared with `doctor`, which may name a path inside it: an administrator put
/// that file there deliberately, so saying which one is broken is the help,
/// not a leak.
pub(crate) const SYSTEM_CONFIG_DIR: &str = "/etc/ego";
/// Overrides `~/.ego` for the user layer (`CLI.md` §7).
const EGO_HOME: &str = "EGO_HOME";
/// Environment overrides, between the flags and the project layers.
const ENV_MODEL: &str = "EGO_MODEL";
const ENV_PROFILE: &str = "EGO_PROFILE";

/// Report discarded source names and setting keys through the caller's output
/// boundary. Values stay out of diagnostics, and protocol stdout stays clean.
pub(crate) fn report_discarded(
    workspace: &Path,
    effective: &Effective,
    diagnostics: &dyn Diagnostics,
) {
    for discarded in &effective.discarded {
        diagnostics.note(&discarded.to_string());
    }
    if effective.posture != ego_core::config::Posture::Bare {
        return;
    }
    for (source, path) in project_layer_paths(workspace) {
        if effective
            .discarded
            .iter()
            .any(|layer| layer.source == source)
        {
            continue;
        }
        // Inspect the path, never the untrusted contents or a symlink target.
        // Empty regular files contain no settings and need no diagnostic.
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && metadata.len() == 0 => {}
            Ok(_) => diagnostics.note(&format!(
                "the {source} configuration at {} is not applied in the bare posture; \
                 any model or sandbox settings in that file have no effect. \
                 Use the user configuration or --sandbox for confinement",
                path.display(),
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => diagnostics.note(&format!(
                "the ignored {source} configuration path {} could not be inspected: \
                 {error}; project configuration does not apply in the bare posture",
                path.display(),
            )),
        }
    }
}

/// Execute the user-scoped configuration management surface.
pub async fn execute(args: &ConfigArgs, out: &Stdout, cancel: &CancelToken) -> RunTermination {
    match execute_inner(args, out, cancel).await {
        Ok(()) => RunTermination::Done,
        Err(termination) => termination,
    }
}

async fn execute_inner(
    args: &ConfigArgs,
    out: &Stdout,
    cancel: &CancelToken,
) -> Result<(), RunTermination> {
    if let ConfigAction::Effective { profile, json } = &args.action {
        let workspace = std::env::current_dir().map_err(|_| RunTermination::Failed {
            message: "the workspace cannot be read".into(),
        })?;
        let perimeter =
            crate::perimeter::observe(&workspace, ego_home().as_deref(), profile.as_deref())
                .map_err(|error| RunTermination::Failed {
                    message: error.to_string(),
                })?;
        let rendered = if *json {
            serde_json::to_string(&perimeter).map_err(|error| RunTermination::Failed {
                message: error.to_string(),
            })?
        } else {
            perimeter.text()
        };
        out.line(&rendered);
        return Ok(());
    }
    let editor = matches!(&args.action, ConfigAction::Edit)
        .then(editor_argv)
        .transpose()?;
    let path = ego_home()
        .ok_or_else(|| RunTermination::Failed {
            message: "config management needs EGO_HOME or HOME for user configuration".to_owned(),
        })?
        .join("config.toml");
    let store = OsUserConfigStore::new(Arc::new(OsIo), path);
    match &args.action {
        ConfigAction::Effective { .. } => {
            unreachable!("effective listing returns before stored-value management")
        }
        ConfigAction::Get { target, key, json } => {
            let snapshot = snapshot(&store, target.clone())?;
            let value = snapshot
                .values
                .into_iter()
                .find_map(|(candidate, value)| (candidate == *key).then_some(value).flatten())
                .or_else(|| {
                    (*key == ego_core::config::ManagedConfigKey::Roots)
                        .then(|| ManagedConfigValue::Roots(Vec::new()))
                })
                .ok_or_else(|| RunTermination::Failed {
                    message: format!("config key `{key}` is absent from the selected user target"),
                })?;
            if *json {
                render_json(target, std::iter::once((*key, value)), out)?;
            } else {
                out.line(&value.to_toml());
            }
        }
        ConfigAction::List { target, json } => {
            let snapshot = snapshot(&store, target.clone())?;
            let values = snapshot
                .values
                .into_iter()
                .filter_map(|(key, value)| value.map(|value| (key, value)))
                .collect::<Vec<_>>();
            if *json {
                render_json(target, values, out)?;
            } else if values.is_empty() {
                // The ADR fixes an empty text `ls` as empty stdout, so the
                // explanation goes to the diagnostic channel rather than
                // becoming a row no reader of this format expects.
                out.note(&empty_listing(
                    "no user configuration values are set",
                    "ego config set KEY=VALUE",
                ));
            } else {
                for (key, value) in values {
                    out.line(&format!(
                        "{}\t{}",
                        key.as_str(),
                        escape_field(&value.to_toml())
                    ));
                }
            }
        }
        ConfigAction::Root {
            target,
            path,
            access,
        } => {
            let result = store
                .mutate(UserConfigMutation::Root {
                    target: target.clone(),
                    path: path.clone(),
                    access: *access,
                })
                .map_err(store_failure)?;
            render_receipt(result, out)?;
        }
        ConfigAction::Set { target, key, value } => {
            let result = store
                .mutate(UserConfigMutation::ManagedSet {
                    target: target.clone(),
                    key: *key,
                    value: value.clone(),
                })
                .map_err(store_failure)?;
            render_receipt(result, out)?;
        }
        ConfigAction::Unset { target, key } => {
            let result = store
                .mutate(UserConfigMutation::ManagedUnset {
                    target: target.clone(),
                    key: *key,
                })
                .map_err(store_failure)?;
            render_receipt(result, out)?;
        }
        ConfigAction::Edit => {
            execute_edit(
                &store,
                out,
                cancel,
                editor.expect("the edit action resolved its editor argv"),
            )
            .await?
        }
    }
    Ok(())
}

async fn execute_edit<I>(
    store: &OsUserConfigStore<I>,
    out: &Stdout,
    cancel: &CancelToken,
    mut argv: Vec<OsString>,
) -> Result<(), RunTermination>
where
    I: std::ops::Deref + Send + Sync,
    I::Target: StoreIo,
{
    let edit = store.prepare_edit().map_err(store_failure)?;
    let candidate_path = edit.candidate_path().to_owned();
    output::note(&format!(
        "config editor candidate: {}",
        candidate_path.display()
    ));
    argv.push(candidate_path.as_os_str().to_owned());

    if let Err(error) = run_editor_process(argv, cancel.clone()).await {
        let cleanup = store.discard_edit(edit).err();
        if cancel.is_cancelled() {
            let message = match cleanup {
                Some(cleanup) => format!(
                    "config edit was cancelled, but candidate cleanup failed at {}: {cleanup}",
                    candidate_path.display()
                ),
                None => "config edit was cancelled; its candidate was removed".to_owned(),
            };
            output::note(&message);
            return Err(crate::run::cancelled_termination(cancel));
        }
        let message = match cleanup {
            Some(cleanup) => format!(
                "{error}; candidate cleanup failed at {}: {cleanup}",
                candidate_path.display()
            ),
            None => format!("{error}; config editor candidate was removed"),
        };
        return Err(RunTermination::Failed { message });
    }

    if cancel.is_cancelled() {
        let cleanup = store.discard_edit(edit).err();
        output::note(&match cleanup {
            Some(error) => format!(
                "config edit was cancelled, but candidate cleanup failed at {}: {error}",
                candidate_path.display()
            ),
            None => "config edit was cancelled; its candidate was removed".to_owned(),
        });
        return Err(crate::run::cancelled_termination(cancel));
    }

    let result = store
        .publish_edit(edit)
        .map_err(|error| RunTermination::Failed {
            message: format!(
                "user config edit was not published: {error}; recovery candidate remains at {}",
                candidate_path.display()
            ),
        })?;
    let publication = match &result.publication {
        UserConfigPublication::NotPublished => "not_published",
        UserConfigPublication::PublishedDurably => "published_durably",
        UserConfigPublication::PublishedDurabilityUncertain { diagnostic } => {
            output::note(&format!(
                "config was published but durability is uncertain: {diagnostic}; recovery candidate remains at {}",
                candidate_path.display()
            ));
            "published_durability_uncertain"
        }
    };
    if let Some(warning) = result.candidate_cleanup_warning {
        output::note(&format!(
            "{warning}; recovery candidate remains at {}",
            candidate_path.display()
        ));
    }
    out.line(&format!(
        "{}\tuser\t{publication}",
        if result.changed {
            "changed"
        } else {
            "unchanged"
        }
    ));
    Ok(())
}

fn editor_argv() -> Result<Vec<OsString>, RunTermination> {
    let (name, command) = match editor_environment("VISUAL")? {
        Some(command) => ("VISUAL", command),
        None => match editor_environment("EDITOR")? {
            Some(command) => ("EDITOR", command),
            None => {
                return Err(RunTermination::UsageError {
                    message: "ego config edit needs a nonempty VISUAL or EDITOR command".to_owned(),
                });
            }
        },
    };
    let argv = lex_editor_command(&command).map_err(|error| RunTermination::UsageError {
        message: format!("invalid {name} editor command: {error}"),
    })?;
    if argv.first().is_none_or(|program| program.is_empty()) {
        return Err(RunTermination::UsageError {
            message: format!("{name} editor command does not name an executable"),
        });
    }
    Ok(argv)
}

fn editor_environment(name: &str) -> Result<Option<String>, RunTermination> {
    match std::env::var(name) {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(RunTermination::UsageError {
            message: format!("{name} editor command is not valid UTF-8"),
        }),
    }
}

#[derive(Clone, Copy)]
enum EditorQuote {
    Single,
    Double,
}

fn lex_editor_command(command: &str) -> Result<Vec<OsString>, &'static str> {
    let bytes = command.as_bytes();
    let mut argv = Vec::new();
    let mut word = Vec::new();
    let mut word_started = false;
    let mut quote = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(EditorQuote::Single) => {
                if byte == b'\'' {
                    quote = None;
                } else {
                    word.push(byte);
                }
            }
            Some(EditorQuote::Double) => match byte {
                b'"' => quote = None,
                b'\\' => {
                    index += 1;
                    let Some(escaped) = bytes.get(index) else {
                        return Err("editor command ends with a dangling escape");
                    };
                    word.push(*escaped);
                }
                _ => word.push(byte),
            },
            None => match byte {
                byte if byte.is_ascii_whitespace() => {
                    if word_started {
                        push_editor_word(&mut argv, &mut word);
                        word_started = false;
                    }
                }
                b'\'' => {
                    word_started = true;
                    quote = Some(EditorQuote::Single);
                }
                b'"' => {
                    word_started = true;
                    quote = Some(EditorQuote::Double);
                }
                b'\\' => {
                    word_started = true;
                    index += 1;
                    let Some(escaped) = bytes.get(index) else {
                        return Err("editor command ends with a dangling escape");
                    };
                    word.push(*escaped);
                }
                _ => {
                    word_started = true;
                    word.push(byte);
                }
            },
        }
        index += 1;
    }
    if quote.is_some() {
        return Err("editor command contains an unmatched quote");
    }
    if word_started {
        push_editor_word(&mut argv, &mut word);
    }
    Ok(argv)
}

fn push_editor_word(argv: &mut Vec<OsString>, word: &mut Vec<u8>) {
    let word = String::from_utf8(std::mem::take(word))
        .expect("removing ASCII editor syntax preserves UTF-8");
    argv.push(OsString::from(word));
}

fn snapshot(
    store: &impl UserConfigStore,
    target: UserConfigTarget,
) -> Result<UserConfigSnapshot, RunTermination> {
    store.snapshot(target).map_err(store_failure)
}

fn store_failure(error: impl std::fmt::Display) -> RunTermination {
    RunTermination::Failed {
        message: format!("user config operation failed before publication: {error}"),
    }
}

fn render_json(
    target: &UserConfigTarget,
    values: impl IntoIterator<Item = (ego_core::config::ManagedConfigKey, ManagedConfigValue)>,
    out: &Stdout,
) -> Result<(), RunTermination> {
    let mut rendered = Vec::new();
    for (key, value) in values {
        let toml = value
            .to_toml()
            .parse::<toml::Value>()
            .map_err(|_| RunTermination::Failed {
                message: format!("config key `{key}` could not be rendered"),
            })?;
        let json = serde_json::to_value(toml).map_err(|error| RunTermination::Failed {
            message: format!("config key `{key}` could not be rendered: {error}"),
        })?;
        let key = serde_json::to_string(key.as_str()).expect("a string is always valid JSON");
        let value = serde_json::to_string(&json).map_err(|error| RunTermination::Failed {
            message: format!("config value could not be rendered: {error}"),
        })?;
        rendered.push(format!("{key}:{value}"));
    }
    let profile = match target.profile() {
        Some(profile) => serde_json::to_string(profile.as_str())
            .expect("a validated profile string is always valid JSON"),
        None => "null".to_owned(),
    };
    let line = format!(
        "{{\"v\":1,\"scope\":\"user\",\"profile\":{profile},\"values\":{{{}}}}}",
        rendered.join(",")
    );
    out.line(&line);
    Ok(())
}

fn render_receipt(result: UserConfigMutationResult, out: &Stdout) -> Result<(), RunTermination> {
    let (key, value) = match result.facts {
        UserConfigMutationFacts::Root { path, access } => (
            ego_core::config::ManagedConfigKey::Roots,
            format!(
                "{} {}",
                match access {
                    Some(ego_core::permit::RootAccess::Read) => "read",
                    Some(ego_core::permit::RootAccess::ReadWrite) => "read-write",
                    None => "removed",
                },
                path.display()
            ),
        ),
        UserConfigMutationFacts::Set { key, value } => (key, value.to_toml()),
        UserConfigMutationFacts::Unset { key } => (key, "<absent>".to_owned()),
        _ => {
            return Err(RunTermination::Failed {
                message: "config mutation returned unrelated transaction facts".to_owned(),
            });
        }
    };
    let target = user_target(&result.target);
    let (publication, warning) = match result.publication {
        UserConfigPublication::NotPublished => ("not_published", None),
        UserConfigPublication::PublishedDurably => ("published_durably", None),
        UserConfigPublication::PublishedDurabilityUncertain { diagnostic } => {
            ("published_durability_uncertain", Some(diagnostic))
        }
    };
    out.line(&format!(
        "{}\t{}\t{}\t{}\t{}",
        if result.changed {
            "changed"
        } else {
            "unchanged"
        },
        escape_field(&target),
        key.as_str(),
        escape_field(&value),
        publication
    ));
    if let Some(warning) = warning {
        output::note(&format!(
            "config was published but durability is uncertain: {warning}"
        ));
    }
    Ok(())
}

pub(crate) fn escape_field(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

pub(crate) fn user_target(target: &UserConfigTarget) -> String {
    match target {
        UserConfigTarget::Base => "user".to_owned(),
        UserConfigTarget::Profile(profile) => format!(
            "user:profile={}",
            serde_json::to_string(profile.as_str()).expect("a string is always valid JSON")
        ),
    }
}

/// The user layer's directory: `$EGO_HOME`, else `~/.ego`.
pub fn ego_home() -> Option<PathBuf> {
    if let Ok(home) = std::env::var(EGO_HOME) {
        return Some(PathBuf::from(home));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ego"))
}

/// Authentication configuration for `ego auth`.
///
/// The command has no workspace/trust flags, so only the system and user
/// layers are eligible. Parsing and precedence are nevertheless exactly the
/// same [`ConfigLayer`] and [`resolve`] path generation uses; authentication
/// must not grow a second interpretation of `config.toml`.
pub fn auth() -> Result<std::collections::HashMap<String, AuthProviderLayer>, UsageError> {
    let mut found = Vec::new();
    if let Some(system) = layer(Source::System, Path::new(SYSTEM_CONFIG))? {
        found.push(system);
    }
    if let Some(home) = ego_home()
        && let Some(user) = layer(Source::User, &home.join("config.toml"))?
    {
        found.push(user);
    }
    resolve(&found, Trust::Untrusted, None)
        .map(|effective| effective.auth)
        .map_err(|err| UsageError(err.to_string()))
}

/// The complete non-interactive hook-inspection input. Project and local
/// configuration are ineligible and therefore are never opened.
pub(crate) fn hook_listing_layers() -> Result<Vec<Sourced<ConfigLayer>>, UsageError> {
    let mut found = Vec::new();
    if let Some(system) = layer(Source::System, Path::new(SYSTEM_CONFIG))? {
        found.push(system);
    }
    if let Some(home) = ego_home()
        && let Some(user) = layer(Source::User, &home.join("config.toml"))?
    {
        found.push(user);
    }
    Ok(found)
}

/// One captured administrator authority and the user document it governs.
pub(crate) struct RuleManagementConfig {
    pub(crate) system: Ruleset,
    pub(crate) user_path: PathBuf,
}

/// Resolve only administrator rules for one explicit management target.
///
/// A marker profile makes a user-only profile selectable without contributing
/// any permissions. The stored user lists are read separately by the existing
/// store port, or atomically returned by its mutation transaction.
pub(crate) fn rule_management(
    target: &UserConfigTarget,
) -> Result<RuleManagementConfig, UsageError> {
    let user_path = ego_home()
        .ok_or_else(|| {
            UsageError("ego rules needs EGO_HOME or HOME for user configuration".into())
        })?
        .join("config.toml");
    let mut found = Vec::new();
    if let Some(system) = layer(Source::System, Path::new(SYSTEM_CONFIG))? {
        found.push(system);
    }
    let profile = target.profile();
    if let Some(profile) = profile {
        let mut marker = ConfigLayer::default();
        marker
            .profile
            .insert(profile.as_str().to_owned(), ProfileLayer::default());
        found.push(Sourced {
            source: Source::User,
            value: marker,
        });
    }
    let system = resolve(&found, Trust::Untrusted, profile.map(ProfileName::as_str))
        .map_err(|error| UsageError(error.to_string()))?
        .permissions;
    Ok(RuleManagementConfig { system, user_path })
}

/// The system/user authority used by process-wide MCP management commands.
///
/// `ego mcp` has no workspace or trust flags. It therefore manages only the
/// two non-ambient layers that are meaningful outside a run. The selected
/// `EGO_PROFILE`, when present, still goes through core's ordinary profile
/// resolution and names the exact user target a mutation must compare.
pub(crate) struct McpManagementConfig {
    pub(crate) effective: Effective,
    pub(crate) user: ConfigLayer,
    pub(crate) profile: Option<ProfileName>,
    pub(crate) user_path: PathBuf,
}

impl McpManagementConfig {
    /// The exact user-owned entry observed before a compare-and-set mutation.
    pub(crate) fn user_entry(
        &self,
        target: &UserConfigTarget,
        name: &McpServerName,
    ) -> Option<&McpLayerEntry> {
        match target {
            UserConfigTarget::Base => self.user.mcp.servers.get(name),
            UserConfigTarget::Profile(profile) => self
                .user
                .profile
                .get(profile.as_str())
                .and_then(|layer| layer.mcp.servers.get(name)),
        }
    }

    /// The target selected for an add operation.
    pub(crate) fn selected_user_target(&self) -> UserConfigTarget {
        self.profile
            .clone()
            .map(UserConfigTarget::Profile)
            .unwrap_or(UserConfigTarget::Base)
    }
}

/// Resolve the complete non-ambient MCP management view through core's one
/// precedence implementation while retaining the exact user layer for CAS.
pub(crate) fn mcp_management() -> Result<McpManagementConfig, UsageError> {
    let user_path = ego_home()
        .ok_or_else(|| UsageError("ego mcp needs EGO_HOME or HOME for user configuration".into()))?
        .join("config.toml");
    let mut found = Vec::new();
    if let Some(system) = layer(Source::System, Path::new(SYSTEM_CONFIG))? {
        found.push(system);
    }
    let user = match layer(Source::User, &user_path)? {
        Some(user) => {
            let value = user.value.clone();
            found.push(user);
            value
        }
        None => ConfigLayer::default(),
    };
    let profile = std::env::var(ENV_PROFILE)
        .ok()
        .map(|name| {
            ProfileName::parse(&name)
                .map_err(|error| UsageError(format!("invalid {ENV_PROFILE}: {error}")))
        })
        .transpose()?;
    let effective = resolve(
        &found,
        Trust::Untrusted,
        profile.as_ref().map(ProfileName::as_str),
    )
    .map_err(|error| UsageError(error.to_string()))?;
    Ok(McpManagementConfig {
        effective,
        user,
        profile,
        user_path,
    })
}

/// Read one layer, or nothing if the file is not there.
///
/// A file that exists and cannot be *read* is an error, not an absence: an
/// unreadable `/etc/ego/config.toml` may be the layer carrying the ceilings,
/// and running as though an administrator had configured nothing is the
/// failure mode §4.5 was written against.
fn layer(source: Source, path: &Path) -> Result<Option<Sourced<ConfigLayer>>, UsageError> {
    match std::fs::read_to_string(path) {
        Ok(text) => ConfigLayer::parse(source, &text)
            .map(Some)
            .map_err(|err: ConfigError| UsageError(format!("{}: {err}", path.display()))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(UsageError(format!(
            "{} could not be read: {err}",
            path.display()
        ))),
    }
}

/// Every layer this invocation is subject to, in no particular order —
/// `resolve` sorts them by rank itself, and depending on this order would be
/// a second, silent precedence rule.
///
/// Untrusted project and local files are never opened. The resolver also applies
/// the trust gate to injected layers; diagnostics can inspect excluded paths'
/// metadata without opening their contents.
pub fn layers(
    workspace: &Path,
    args: &Args,
    trust: Trust,
) -> Result<Vec<Sourced<ConfigLayer>>, UsageError> {
    let mut found = base_layers(workspace, trust)?;
    if let Some(agent_file) = &args.agent_file {
        found.push(agent_file.layer()?);
    }
    found.push(args_layer(args)?);
    Ok(found)
}

/// One startup snapshot of every non-request configuration input.
#[derive(Clone)]
pub(crate) struct FrozenRunConfig {
    layers: Vec<Sourced<ConfigLayer>>,
    profile: Option<SelectedProfile>,
    trust: Trust,
}

/// One process-captured profile selection. Reading the environment again
/// after this value exists would create a second selection authority.
#[derive(Clone)]
pub(crate) struct SelectedProfile {
    name: String,
    source: Source,
}

impl SelectedProfile {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn as_resolver_input(&self) -> (&str, Source) {
        (&self.name, self.source)
    }
}

/// Capture the environment fallback once at direct-host entry. A flag value
/// is already tagged by argument construction and always wins.
pub fn capture_profile(args: &mut Args) {
    if args.profile.is_some() {
        return;
    }
    if let Ok(name) = std::env::var(ENV_PROFILE) {
        args.profile = Some(name);
        args.profile_source = Some(Source::Env);
    }
}

/// Capture the metadata command's only ambient selector once at process entry.
pub fn capture_metadata_profile(args: &mut SessionMetadataArgs) {
    if let Ok(name) = std::env::var(ENV_PROFILE) {
        args.profile = Some(name);
    }
}

/// Resolve the immutable goal ceiling snapshot from system and user authority.
/// Project/local files are never opened because metadata has no trusted seat.
pub(crate) fn metadata_ceilings(
    workspace: &Path,
    args: &SessionMetadataArgs,
) -> Result<Ceilings, UsageError> {
    let layers = base_layers(workspace, Trust::Untrusted)?;
    resolve_selected(
        &layers,
        Trust::Untrusted,
        args.profile
            .as_deref()
            .map(|profile| (profile, Source::Env)),
    )
    .map(|effective| effective.ceilings)
    .map_err(|error| UsageError(error.to_string()))
}

impl FrozenRunConfig {
    pub(crate) fn capture(workspace: &Path, args: &Args, trust: Trust) -> Result<Self, UsageError> {
        Ok(Self::from_layers(
            base_layers(workspace, trust)?,
            args,
            trust,
        ))
    }

    /// Pin an already captured layer authority for host-facing preparation.
    pub(crate) fn from_layers(
        layers: Vec<Sourced<ConfigLayer>>,
        args: &Args,
        trust: Trust,
    ) -> Self {
        Self {
            layers,
            profile: profile(args),
            trust,
        }
    }

    /// The posture this capture pinned.
    ///
    /// A child runs at its parent's posture, so this is the answer for every
    /// consumer that asks one — including the ones that read the posture
    /// rather than the layers, which is how a child came to resolve project
    /// configuration and admit no hooks from it.
    pub(crate) const fn trust(&self) -> Trust {
        self.trust
    }

    /// The layers this capture resolves against, composed exactly as [`layers`]
    /// composes a fresh run's: the pinned base, then this request's agent file
    /// and its flags.
    pub(crate) fn layers(&self, args: &Args) -> Result<Vec<Sourced<ConfigLayer>>, UsageError> {
        let mut layers = self.layers.clone();
        if let Some(agent_file) = &args.agent_file {
            layers.push(agent_file.layer()?);
        }
        layers.push(args_layer(args)?);
        Ok(layers)
    }

    pub(crate) fn resolve(&self, args: &Args) -> Result<Effective, UsageError> {
        self.resolve_layers(&self.layers(args)?, args)
    }

    /// Resolve layers this capture already composed, so a caller that needs
    /// both does not compose them twice.
    pub(crate) fn resolve_layers(
        &self,
        layers: &[Sourced<ConfigLayer>],
        args: &Args,
    ) -> Result<Effective, UsageError> {
        let request_profile = profile(args);
        let selected_profile = request_profile.as_ref().or(self.profile.as_ref());
        resolve_selected(
            layers,
            self.trust,
            selected_profile.map(SelectedProfile::as_resolver_input),
        )
        .map_err(|error| UsageError(error.to_string()))
    }

    pub(crate) fn profile(&self) -> Option<&str> {
        self.profile.as_ref().map(SelectedProfile::name)
    }
}

/// Resolve the explicit invocation MCP selection before any session or
/// provider exists. Input order decides which diagnostic wins; the returned
/// authority is canonical by server name.
pub(crate) fn selected_mcp(
    args: &Args,
    effective: &Effective,
    cwd: &Path,
) -> Result<Vec<(McpServerName, McpServerConfig, McpConfigProvenance)>, UsageError> {
    let mut selected = Vec::with_capacity(args.mcp.len());
    let mut names = std::collections::BTreeSet::new();
    for selector in &args.mcp {
        let (name, server, provenance) = match selector {
            McpSelector::Named(name) => match effective.mcp.entry(name) {
                Some(ResolvedMcpEntry::Configured { server, provenance }) => {
                    (name.clone(), server.clone(), provenance.clone())
                }
                Some(ResolvedMcpEntry::Tombstone { provenance }) => {
                    return Err(UsageError(format!(
                        "MCP server `{name}` is masked by {}",
                        mcp_provenance(provenance)
                    )));
                }
                None => {
                    return Err(UsageError(format!("MCP server `{name}` is not configured")));
                }
            },
            McpSelector::File(path) => {
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    cwd.join(path)
                };
                let bytes = std::fs::read(&path).map_err(|error| {
                    UsageError(format!(
                        "MCP invocation file {} could not be read: {error}",
                        path.display()
                    ))
                })?;
                let decoded: InvocationMcpServer =
                    serde_json::from_slice(&bytes).map_err(|error| {
                        UsageError(format!(
                            "MCP invocation file {} is not one strict server object: {error}",
                            path.display()
                        ))
                    })?;
                let (name, server) = decoded.into_config()?;
                if effective.mcp.entry(&name).is_some() {
                    return Err(UsageError(format!(
                        "MCP invocation server `{name}` collides with effective configuration"
                    )));
                }
                (
                    name,
                    server,
                    McpConfigProvenance::Layer {
                        source: Source::Flags,
                    },
                )
            }
        };
        if !names.insert(name.clone()) {
            return Err(UsageError(format!(
                "MCP server `{name}` was selected more than once"
            )));
        }
        selected.push((name, server, provenance));
    }
    selected.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(selected)
}

/// Canonical configured entries from an already resolved effective view.
/// OS-owned live materialization happens in `ego-tools`.
pub(crate) fn configured_mcp(
    effective: &Effective,
) -> Vec<(McpServerName, McpServerConfig, McpConfigProvenance)> {
    effective
        .mcp
        .entries()
        .iter()
        .filter_map(|(name, entry)| match entry {
            ResolvedMcpEntry::Configured { server, provenance } => {
                Some((name.clone(), server.clone(), provenance.clone()))
            }
            ResolvedMcpEntry::Tombstone { .. } => None,
        })
        .collect()
}

fn mcp_provenance(provenance: &McpConfigProvenance) -> String {
    match provenance {
        McpConfigProvenance::Layer { source } => format!("the {source} layer"),
        McpConfigProvenance::Profile { source, name } => {
            format!("the {source} profile `{name}`")
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase", deny_unknown_fields)]
enum InvocationMcpServer {
    Stdio {
        name: McpServerName,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        inherit_env: Vec<McpEnvironmentName>,
    },
    Http {
        name: McpServerName,
        url: McpHttpUrl,
        #[serde(default)]
        auth: McpHttpAuth,
        #[serde(default)]
        allow_insecure_loopback: bool,
    },
}

impl InvocationMcpServer {
    fn into_config(self) -> Result<(McpServerName, McpServerConfig), UsageError> {
        match self {
            Self::Stdio {
                name,
                command,
                args,
                cwd,
                inherit_env,
            } => McpServerConfig::stdio(command, args, cwd, inherit_env)
                .map(|server| (name, server))
                .map_err(|error| UsageError(error.to_string())),
            Self::Http {
                name,
                url,
                auth,
                allow_insecure_loopback,
            } => McpHttpConfig::new(url, auth, allow_insecure_loopback)
                .map(McpServerConfig::Http)
                .map(|server| (name, server))
                .map_err(|error| UsageError(error.to_string())),
        }
    }
}

fn project_layer_paths(workspace: &Path) -> [(Source, PathBuf); 2] {
    [
        (Source::Project, workspace.join(".ego/config.toml")),
        (Source::Local, workspace.join(".ego/config.local.toml")),
    ]
}

fn base_layers(workspace: &Path, trust: Trust) -> Result<Vec<Sourced<ConfigLayer>>, UsageError> {
    base_layers_in(workspace, trust, ego_home().as_deref())
}

pub(crate) fn base_layers_in(
    workspace: &Path,
    trust: Trust,
    home: Option<&Path>,
) -> Result<Vec<Sourced<ConfigLayer>>, UsageError> {
    // Which files this invocation is subject to, decided before any of them is
    // opened: whether a layer is *eligible* is a question about the posture and
    // the environment, and answering it separately from reading keeps the trust
    // gate below in one place instead of once per layer.
    let mut files = vec![(Source::System, PathBuf::from(SYSTEM_CONFIG))];
    if let Some(home) = home {
        files.push((Source::User, home.join("config.toml")));
    }
    // Open configuration only in a trusted workspace. Diagnostics may inspect
    // metadata for excluded paths, but never read or parse their contents.
    if trust == Trust::Trusted {
        files.extend(project_layer_paths(workspace));
    }

    let mut found = Vec::new();
    for (source, path) in files {
        // `extend` and not `push`: a file that is not there contributes no
        // layer, which is the `None` `layer` returns.
        found.extend(layer(source, &path)?);
    }
    found.extend(env_layer()?);
    Ok(found)
}

/// `EGO_MODEL` and friends, as a layer of their own so they take their place
/// in one precedence rule rather than being consulted ad hoc wherever a value
/// is needed.
fn env_layer() -> Result<Option<Sourced<ConfigLayer>>, UsageError> {
    let mut toml = String::new();
    if let Ok(model) = std::env::var(ENV_MODEL) {
        toml.push_str(&format!("model = {}\n", quote(&model)));
    }
    if toml.is_empty() {
        return Ok(None);
    }
    ConfigLayer::parse(Source::Env, &toml)
        .map(Some)
        .map_err(|err| UsageError(format!("the environment: {err}")))
}

/// The command line as a layer.
///
/// `-c K=V` is written into the same TOML the file layers are, so a dotted
/// override is subject to the same strict schema: `-c bogus=1` is refused by
/// name exactly as a config file's unknown key would be, rather than being
/// accepted because it came from a flag.
fn args_layer(args: &Args) -> Result<Sourced<ConfigLayer>, UsageError> {
    let mut toml = selection_flags(args.model.as_deref(), args.thinking);
    if args.bare {
        toml.push_str("bare = true\n");
    }
    // `--budget` is the flags layer of the ceiling, not a value parsed on the
    // side. `2026-08-21-a-run-with-no-ceiling.md` §4: an administrator's
    // system layer is the only source of non-relaxable ceilings, and it can
    // only clamp a figure that went through the same resolution it did.
    if let Some(budget) = &args.budget {
        toml.push_str(&format!("budget = {}\n", quote(budget)));
    }
    if let Some(mode) = args.mode {
        toml.push_str(&format!("mode = {}\n", quote(mode.as_str())));
    }
    if let Some(sandbox) = args.sandbox {
        toml.push_str(&format!("sandbox = {}\n", quote(&sandbox.to_string())));
    }
    // Written as **dotted keys**, not a `[permissions]` table header. A header
    // captures every key after it, and `-c` overrides are appended below —
    // `-c max_turns=3` after a header would land inside `permissions` and be
    // refused as an unknown key, naming the wrong culprit.
    //
    // Emitted only when a rule was actually given. `resolve` replaces a
    // layer's whole `permissions` block, so a `permissions.allow = []` written
    // whenever the flag is absent would make every run erase the deny list a
    // config file set.
    for (key, rules) in [("allow", &args.allow), ("deny", &args.deny)] {
        if rules.is_empty() {
            continue;
        }
        let list = rules.iter().map(|rule| quote(rule)).collect::<Vec<_>>();
        toml.push_str(&format!("permissions.{key} = [{}]\n", list.join(", ")));
    }
    for entry in &args.set {
        let (key, value) = entry.split_once('=').ok_or_else(|| {
            UsageError(format!(
                "`-c {entry}` is not a config override: write it as `-c key=value`, \
                 with the value in TOML — `-c 'model=\"ollama/qwen3\"'`"
            ))
        })?;
        toml.push_str(&format!("{key} = {value}\n"));
    }
    // KNOWN, AND DELIBERATELY LEFT: this prefix doubles. `ConfigError` renders
    // its own origin (`error.rs:27`: "invalid config in {source} layer: …"),
    // so the operator reads "the command line: invalid config in flags layer:
    // …" — two names for one place, neither wrong.
    //
    // Two people have now found this and proposed the same fix: drop one of
    // the two. Do not, in isolation. The flags layer is the only `Source` an
    // operator never *writes* — there is no file to open — so the two halves
    // are not redundant to a reader who does not already know that `flags` and
    // "the command line" are the same layer. The fix worth making is to give
    // `Source`'s `Display` a form that reads correctly in a sentence, which
    // changes every config diagnostic and belongs with `Source::rank` going
    // public (`docs/decisions/2026-08-21-a-deny-that-names-who-can-lift-it.md`
    // "Consequences"), not with whichever caller noticed it.
    ConfigLayer::parse(Source::Flags, &toml)
        .map_err(|err| UsageError(format!("the command line: {err}")))
}

/// The model-selection flags as one precedence layer.
///
/// An absent value contributes no key, so a user or profile default remains
/// authoritative instead of being overwritten by a parser default.
pub fn flags_layer(
    model: Option<&str>,
    thinking: Option<ego_core::generator::types::ThinkingLevel>,
) -> Sourced<ConfigLayer> {
    ConfigLayer::parse(Source::Flags, &selection_flags(model, thinking))
        .expect("typed model-selection flags form a valid config layer")
}

fn selection_flags(
    model: Option<&str>,
    thinking: Option<ego_core::generator::types::ThinkingLevel>,
) -> String {
    let mut toml = String::new();
    if let Some(model) = model {
        toml.push_str(&format!("model = {}\n", quote(model)));
    }
    if let Some(thinking) = thinking {
        toml.push_str(&format!("thinking = {}\n", quote(thinking.as_str())));
    }
    toml
}

/// The active profile: `--profile` beats `EGO_PROFILE`, per §7's precedence.
pub(crate) fn profile(args: &Args) -> Option<SelectedProfile> {
    args.profile.clone().map(|name| SelectedProfile {
        name,
        source: args
            .profile_source
            .expect("a captured profile carries its selection source"),
    })
}

/// A TOML string literal. `toml`'s own serialiser would do this, and reaching
/// for it to quote one string would pull a value model in to answer a lexical
/// question.
fn quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::config::{ModelRef, Source, Trust};

    fn run_args(profile: &str) -> Args {
        let argv = ["ego", "run", "--profile", profile, "Ask"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let crate::cli::Invocation::Run(args) =
            crate::cli::parse(argv).expect("profile fixture invocation must parse")
        else {
            panic!("the profile fixture invocation must be a headless run");
        };
        *args
    }

    #[test]
    fn request_profile_overrides_captured_startup_profile_and_records_flags_authority() {
        let owner = tempfile::tempdir().expect("profile fixture owner");
        let workspace = owner.path();
        let ego = workspace.join(".ego");
        std::fs::create_dir(&ego).expect("profile fixture config directory");
        std::fs::write(
            ego.join("config.toml"),
            r#"[profile.regression_case]
model = "startup/model"

[profile.regression_case]
model = "request/model"
"#,
        )
        .expect("profile fixture config");

        let startup = run_args("regression_case");
        let frozen = FrozenRunConfig::capture(workspace, &startup, Trust::Trusted)
            .expect("startup config must be captured");
        assert_eq!(frozen.profile(), Some("regression_case"));

        let request = run_args("regression_case");
        let effective = frozen
            .resolve(&request)
            .expect("request profile must resolve");
        assert_eq!(effective.model, Some(ModelRef("request/model".to_owned())));

        let active = effective
            .active_profile
            .expect("request profile provenance must be present");
        assert_eq!(active.name, "regression_case");
        assert_eq!(active.selected_by, Source::Flags);
        assert!(!active.definitions.is_empty());
        assert!(active.definitions.contains(Source::Project));
    }
}
