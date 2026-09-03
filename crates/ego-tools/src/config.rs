//! OS adapter for the atomic user-configuration mutation port.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use ego_core::config::{
    ConfigLayer, ManagedConfigKey, ManagedConfigValue, McpHttpAuth, McpLayerEntry, McpServerConfig,
    Source, UserConfigError, UserConfigMutation, UserConfigMutationResult, UserConfigPublication,
    UserConfigStore, UserConfigTarget,
};
use fd_lock::RwLock;
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value, value};

use crate::store::{AtomicReplacePublication, StoreIo};

/// The real filesystem adapter for one user `config.toml` authority.
pub struct OsUserConfigStore<I> {
    io: I,
    path: PathBuf,
}

impl<I> OsUserConfigStore<I> {
    pub fn new(io: I, path: impl AsRef<Path>) -> Self {
        Self {
            io,
            path: path.as_ref().to_path_buf(),
        }
    }
}

impl<I> UserConfigStore for OsUserConfigStore<I>
where
    I: std::ops::Deref + Send + Sync,
    I::Target: StoreIo,
{
    fn mutate(
        &self,
        mutation: UserConfigMutation,
    ) -> Result<UserConfigMutationResult, UserConfigError> {
        mutation.validate()?;
        let target = mutation.target();
        let facts = mutation.facts();

        let lock_path = lock_path_for(&self.path);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(transaction_error)?;
        }
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(transaction_error)?;
        let mut lock = RwLock::new(lock_file);
        let _guard = lock.write().map_err(transaction_error)?;

        let bytes = self.io.read(&self.path).map_err(transaction_error)?;
        let text = match bytes {
            Some(bytes) => String::from_utf8(bytes).map_err(|error| {
                UserConfigError::new(format!("user config is not UTF-8: {error}"))
            })?,
            None => String::new(),
        };
        let parsed = ConfigLayer::parse(Source::User, &text)
            .map_err(|error| UserConfigError::new(format!("invalid user config: {error}")))?;
        let mut document = text
            .parse::<DocumentMut>()
            .map_err(|error| UserConfigError::new(format!("invalid user config TOML: {error}")))?;

        let changed = apply(&mut document, &parsed.value, mutation)?;
        if !changed {
            return Ok(UserConfigMutationResult {
                target,
                facts,
                changed: false,
                publication: UserConfigPublication::NotPublished,
            });
        }
        let published = document.to_string();
        ConfigLayer::parse(Source::User, &published).map_err(|error| {
            UserConfigError::new(format!(
                "mutated user config failed strict validation: {error}"
            ))
        })?;
        let publication = self
            .io
            .replace_atomic_publication(&self.path, published.as_bytes())
            .map_err(transaction_error)?;
        let publication = match publication {
            AtomicReplacePublication::PublishedDurably => UserConfigPublication::PublishedDurably,
            AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } => {
                UserConfigPublication::PublishedDurabilityUncertain { diagnostic }
            }
        };
        Ok(UserConfigMutationResult {
            target,
            facts,
            changed: true,
            publication,
        })
    }
}

fn apply(
    document: &mut DocumentMut,
    parsed: &ConfigLayer,
    mutation: UserConfigMutation,
) -> Result<bool, UserConfigError> {
    match mutation {
        UserConfigMutation::ManagedSet { target, key, value } => {
            if managed_value(parsed, &target, key).as_ref() == Some(&value) {
                return Ok(false);
            }
            apply_managed_set(document, &target, key, &value)?;
            Ok(true)
        }
        UserConfigMutation::ManagedUnset { target, key } => {
            if managed_value(parsed, &target, key).is_none() {
                return Ok(false);
            }
            apply_managed_unset(document, &target, key)?;
            Ok(true)
        }
        UserConfigMutation::ModelAndThinking { model, thinking } => {
            if parsed
                .model
                .as_ref()
                .is_some_and(|current| current.0 == model)
                && parsed.thinking == Some(thinking)
            {
                return Ok(false);
            }
            let root = document.as_table_mut();
            replace_string(root.entry("model").or_insert(Item::None), &model)?;
            replace_string(
                root.entry("thinking").or_insert(Item::None),
                thinking.as_str(),
            )?;
            Ok(true)
        }
        UserConfigMutation::Statusline { segments } => {
            if statusline_matches(parsed, &segments) {
                return Ok(false);
            }
            let mut array = Array::new();
            for segment in &segments {
                array.push(segment.as_str());
            }
            set_table_path(
                document.as_table_mut(),
                managed_path(ManagedConfigKey::StatuslineSegments),
                value(array),
            )?;
            Ok(true)
        }
        UserConfigMutation::McpSet {
            target,
            name,
            expected_user_entry,
            server,
        } => {
            let current = mcp_entry(parsed, &target, &name);
            if current != expected_user_entry.as_ref() {
                return Err(UserConfigError::new(
                    "user config changed before the MCP entry could be set",
                ));
            }
            if matches!(current, Some(McpLayerEntry::Server(current)) if current == &server) {
                return Ok(false);
            }
            let servers = mcp_servers_table_mut(document, &target)?;
            servers.insert(&name.to_string(), Item::Table(server_table(&server)?));
            Ok(true)
        }
        UserConfigMutation::McpDeleteUserEntry {
            target,
            name,
            expected_entry,
        } => {
            let current = mcp_entry(parsed, &target, &name);
            if current != Some(&expected_entry) {
                return Err(UserConfigError::new(
                    "user config changed before the MCP entry could be deleted",
                ));
            }
            let servers = existing_mcp_servers_table_mut(document, &target).ok_or_else(|| {
                UserConfigError::new("the expected MCP entry is absent from the user config")
            })?;
            if servers.remove(&name.to_string()).is_none() {
                return Err(UserConfigError::new(
                    "the expected MCP entry is absent from the user config",
                ));
            }
            Ok(true)
        }
        UserConfigMutation::McpInsertUserTombstone {
            name,
            expected_user_entry,
        } => {
            let current = parsed.mcp.servers.get(&name);
            if current != expected_user_entry.as_ref() {
                return Err(UserConfigError::new(
                    "user config changed before the MCP tombstone could be inserted",
                ));
            }
            let target = mcp_servers_table_mut(document, &UserConfigTarget::Base)?;
            let mut tombstone = Table::new();
            tombstone.insert("removed", value(true));
            target.insert(&name.to_string(), Item::Table(tombstone));
            Ok(true)
        }
    }
}

fn managed_value(
    parsed: &ConfigLayer,
    target: &UserConfigTarget,
    key: ManagedConfigKey,
) -> Option<ManagedConfigValue> {
    match target {
        UserConfigTarget::Base => match key {
            ManagedConfigKey::Model => parsed.model.clone().map(ManagedConfigValue::Model),
            ManagedConfigKey::Thinking => parsed.thinking.map(ManagedConfigValue::Thinking),
            ManagedConfigKey::Mode => parsed.mode.map(ManagedConfigValue::Mode),
            ManagedConfigKey::Sandbox => parsed.sandbox.map(ManagedConfigValue::Sandbox),
            ManagedConfigKey::Budget => parsed.budget.clone().map(ManagedConfigValue::Budget),
            ManagedConfigKey::Bare => parsed.bare.map(ManagedConfigValue::Bare),
            ManagedConfigKey::PermissionsJudge => parsed
                .permissions
                .as_ref()
                .and_then(|permissions| permissions.judge.clone())
                .map(ManagedConfigValue::PermissionsJudge),
            ManagedConfigKey::StatuslineSegments => parsed
                .ui
                .as_ref()
                .and_then(|ui| ui.statusline.as_ref())
                .and_then(|statusline| statusline.segments.clone())
                .map(ManagedConfigValue::StatuslineSegments),
            ManagedConfigKey::SessionDir | ManagedConfigKey::ArtifactDir => None,
        },
        UserConfigTarget::Profile(name) => {
            let profile = parsed.profile.get(name.as_str())?;
            match key {
                ManagedConfigKey::Model => profile.model.clone().map(ManagedConfigValue::Model),
                ManagedConfigKey::Thinking => profile.thinking.map(ManagedConfigValue::Thinking),
                ManagedConfigKey::Mode => profile.mode.map(ManagedConfigValue::Mode),
                ManagedConfigKey::Sandbox => profile.sandbox.map(ManagedConfigValue::Sandbox),
                ManagedConfigKey::Budget => profile.budget.clone().map(ManagedConfigValue::Budget),
                ManagedConfigKey::PermissionsJudge => profile
                    .permissions
                    .as_ref()
                    .and_then(|permissions| permissions.judge.clone())
                    .map(ManagedConfigValue::PermissionsJudge),
                ManagedConfigKey::SessionDir => {
                    profile.session_dir.clone().map(ManagedConfigValue::Path)
                }
                ManagedConfigKey::ArtifactDir => {
                    profile.artifact_dir.clone().map(ManagedConfigValue::Path)
                }
                ManagedConfigKey::Bare | ManagedConfigKey::StatuslineSegments => None,
            }
        }
    }
}

fn statusline_matches(parsed: &ConfigLayer, segments: &[String]) -> bool {
    let Some(current) = parsed
        .ui
        .as_ref()
        .and_then(|ui| ui.statusline.as_ref())
        .and_then(|statusline| statusline.segments.as_ref())
    else {
        return false;
    };
    current.len() == segments.len()
        && current
            .iter()
            .zip(segments)
            .all(|(left, right)| left.as_str() == right)
}

fn apply_managed_set(
    document: &mut DocumentMut,
    target: &UserConfigTarget,
    key: ManagedConfigKey,
    managed: &ManagedConfigValue,
) -> Result<(), UserConfigError> {
    let replacement = managed.to_toml().parse::<Value>().map_err(|error| {
        UserConfigError::new(format!(
            "managed configuration key `{key}` could not be represented as TOML: {error}"
        ))
    })?;
    let table = managed_target_table_mut(document, target)?;
    set_table_path(table, managed_path(key), Item::Value(replacement))
}

fn apply_managed_unset(
    document: &mut DocumentMut,
    target: &UserConfigTarget,
    key: ManagedConfigKey,
) -> Result<(), UserConfigError> {
    let path = managed_path(key);
    let removed = match target {
        UserConfigTarget::Base => remove_table_path(document.as_table_mut(), path)?,
        UserConfigTarget::Profile(name) => {
            let root = document.as_table_mut();
            let remove_profiles = {
                let profiles = existing_table_like_field_mut(root, "profile")?;
                let remove_profile = {
                    let profile = existing_table_like_field_mut(profiles, name.as_str())?;
                    let removed = remove_table_path(profile, path)?;
                    if !removed {
                        return Err(missing_managed_field(target, key));
                    }
                    profile.is_empty()
                };
                if remove_profile {
                    profiles.remove(name.as_str());
                }
                profiles.is_empty()
            };
            if remove_profiles {
                root.remove("profile");
            }
            true
        }
    };
    if removed {
        Ok(())
    } else {
        Err(missing_managed_field(target, key))
    }
}

fn managed_target_table_mut<'a>(
    document: &'a mut DocumentMut,
    target: &UserConfigTarget,
) -> Result<&'a mut dyn TableLike, UserConfigError> {
    match target {
        UserConfigTarget::Base => Ok(document.as_table_mut()),
        UserConfigTarget::Profile(name) => {
            let profiles = table_like_field_mut(document.as_table_mut(), "profile")?;
            table_like_field_mut(profiles, name.as_str())
        }
    }
}

fn managed_path(key: ManagedConfigKey) -> &'static [&'static str] {
    match key {
        ManagedConfigKey::Model => &["model"],
        ManagedConfigKey::Thinking => &["thinking"],
        ManagedConfigKey::Mode => &["mode"],
        ManagedConfigKey::Sandbox => &["sandbox"],
        ManagedConfigKey::Budget => &["budget"],
        ManagedConfigKey::Bare => &["bare"],
        ManagedConfigKey::PermissionsJudge => &["permissions", "judge"],
        ManagedConfigKey::StatuslineSegments => &["ui", "statusline", "segments"],
        ManagedConfigKey::SessionDir => &["session_dir"],
        ManagedConfigKey::ArtifactDir => &["artifact_dir"],
    }
}

fn set_table_path(
    table: &mut dyn TableLike,
    path: &[&str],
    replacement: Item,
) -> Result<(), UserConfigError> {
    let Some((field, rest)) = path.split_first() else {
        return Err(UserConfigError::new(
            "managed configuration field path is empty",
        ));
    };
    if rest.is_empty() {
        let target = table.entry(field).or_insert(Item::None);
        replace_item_preserving_decor(target, replacement);
        return Ok(());
    }
    let nested = table_like_field_mut(table, field)?;
    set_table_path(nested, rest, replacement)
}

fn remove_table_path(table: &mut dyn TableLike, path: &[&str]) -> Result<bool, UserConfigError> {
    let Some((field, rest)) = path.split_first() else {
        return Err(UserConfigError::new(
            "managed configuration field path is empty",
        ));
    };
    if rest.is_empty() {
        return Ok(table.remove(field).is_some());
    }
    let Some(item) = table.get_mut(field) else {
        return Ok(false);
    };
    let (removed, prune) = {
        let nested = item
            .as_table_like_mut()
            .ok_or_else(|| UserConfigError::new(format!("user config `{field}` is not a table")))?;
        let removed = remove_table_path(nested, rest)?;
        (removed, nested.is_empty())
    };
    if removed && prune {
        table.remove(field);
    }
    Ok(removed)
}

fn table_like_field_mut<'a>(
    table: &'a mut dyn TableLike,
    field: &str,
) -> Result<&'a mut dyn TableLike, UserConfigError> {
    if !table.contains_key(field) {
        let mut nested = Table::new();
        nested.set_implicit(true);
        table.insert(field, Item::Table(nested));
    }
    existing_table_like_field_mut(table, field)
}

fn existing_table_like_field_mut<'a>(
    table: &'a mut dyn TableLike,
    field: &str,
) -> Result<&'a mut dyn TableLike, UserConfigError> {
    table
        .get_mut(field)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| UserConfigError::new(format!("user config `{field}` is not a table")))
}

fn missing_managed_field(target: &UserConfigTarget, key: ManagedConfigKey) -> UserConfigError {
    let target = match target {
        UserConfigTarget::Base => "base user configuration".to_owned(),
        UserConfigTarget::Profile(name) => format!("user profile `{name}`"),
    };
    UserConfigError::new(format!(
        "managed configuration key `{key}` is absent from {target}"
    ))
}

fn mcp_entry<'a>(
    parsed: &'a ConfigLayer,
    target: &UserConfigTarget,
    name: &ego_core::config::McpServerName,
) -> Option<&'a McpLayerEntry> {
    match target {
        UserConfigTarget::Base => parsed.mcp.servers.get(name),
        UserConfigTarget::Profile(profile) => parsed
            .profile
            .get(profile.as_str())
            .and_then(|layer| layer.mcp.servers.get(name)),
    }
}

fn mcp_servers_table_mut<'a>(
    document: &'a mut DocumentMut,
    target: &UserConfigTarget,
) -> Result<&'a mut Table, UserConfigError> {
    let root = document.as_table_mut();
    let mcp = match target {
        UserConfigTarget::Base => table_field_mut(root, "mcp")?,
        UserConfigTarget::Profile(profile) => {
            let profiles = table_field_mut(root, "profile")?;
            let profile = table_field_mut(profiles, profile.as_str())?;
            table_field_mut(profile, "mcp")?
        }
    };
    table_field_mut(mcp, "servers")
}

fn existing_mcp_servers_table_mut<'a>(
    document: &'a mut DocumentMut,
    target: &UserConfigTarget,
) -> Option<&'a mut Table> {
    let root = document.as_table_mut();
    let mcp = match target {
        UserConfigTarget::Base => root.get_mut("mcp")?.as_table_mut()?,
        UserConfigTarget::Profile(profile) => root
            .get_mut("profile")?
            .as_table_mut()?
            .get_mut(profile.as_str())?
            .as_table_mut()?
            .get_mut("mcp")?
            .as_table_mut()?,
    };
    mcp.get_mut("servers")?.as_table_mut()
}

fn table_field_mut<'a>(
    table: &'a mut Table,
    field: &str,
) -> Result<&'a mut Table, UserConfigError> {
    if !table.contains_key(field) {
        let mut nested = Table::new();
        nested.set_implicit(true);
        table.insert(field, Item::Table(nested));
    }
    table
        .get_mut(field)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| UserConfigError::new(format!("user config `{field}` is not a table")))
}

fn server_table(server: &McpServerConfig) -> Result<Table, UserConfigError> {
    let mut table = Table::new();
    match server {
        McpServerConfig::Stdio(stdio) => {
            table.insert("transport", value("stdio"));
            table.insert("command", value(stdio.command.as_str()));
            if !stdio.args.is_empty() {
                table.insert("args", string_array(&stdio.args));
            }
            if let Some(cwd) = &stdio.cwd {
                let cwd = cwd.to_str().ok_or_else(|| {
                    UserConfigError::new("MCP stdio working directory is not valid UTF-8")
                })?;
                table.insert("cwd", value(cwd));
            }
            if !stdio.inherit_env.is_empty() {
                let names = stdio
                    .inherit_env
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                table.insert("inherit_env", string_array(&names));
            }
        }
        McpServerConfig::Http(http) => {
            table.insert("transport", value("http"));
            table.insert("url", value(http.url.to_string()));
            if http.auth == McpHttpAuth::Oauth {
                table.insert("auth", value("oauth"));
            }
            if http.allow_insecure_loopback {
                table.insert("allow_insecure_loopback", value(true));
            }
        }
    }
    Ok(table)
}

fn string_array(values: &[String]) -> Item {
    let mut array = Array::new();
    for value in values {
        array.push(value.as_str());
    }
    value(array)
}

/// Parse the scalar through `toml_edit` so the replacement keeps the literal
/// single-quoted representation used by an existing hand-authored config.
fn replace_string(target: &mut Item, text: &str) -> Result<(), UserConfigError> {
    let uses_literal_quotes = matches!(
        target.as_value(),
        Some(Value::String(formatted))
            if formatted
                .as_repr()
                .and_then(|repr| repr.as_raw().as_str())
                .is_some_and(|repr| repr.starts_with('\''))
    );
    let replacement = if uses_literal_quotes && !text.contains(['\'', '\n', '\r']) {
        let literal = format!("'{text}'");
        let value = literal.parse::<Value>().map_err(|error| {
            UserConfigError::new(format!("config string could not be represented: {error}"))
        })?;
        Item::Value(value)
    } else {
        value(text)
    };
    replace_item_preserving_decor(target, replacement);
    Ok(())
}

fn replace_item_preserving_decor(target: &mut Item, mut replacement: Item) {
    let decor = target.as_value().map(|value| value.decor().clone());
    if let (Some(decor), Some(value)) = (decor, replacement.as_value_mut()) {
        *value.decor_mut() = decor;
    }
    *target = replacement;
}

fn lock_path_for(path: &Path) -> PathBuf {
    let mut lock = OsString::from(path.as_os_str());
    lock.push(".lock");
    PathBuf::from(lock)
}

fn transaction_error(error: std::io::Error) -> UserConfigError {
    UserConfigError::new(format!("user config transaction failed: {error}"))
}
