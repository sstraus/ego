//! OS adapter for the atomic user-configuration mutation port.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use ego_core::config::{
    ConfigLayer, McpHttpAuth, McpLayerEntry, McpServerConfig, Source, UserConfigError,
    UserConfigMutation, UserConfigPublication, UserConfigStore, UserMcpSlot,
};
use fd_lock::RwLock;
use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

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
    ) -> Result<UserConfigPublication, UserConfigError> {
        mutation.validate()?;

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

        apply(&mut document, &parsed.value, mutation)?;
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
        Ok(match publication {
            AtomicReplacePublication::PublishedDurably => UserConfigPublication::PublishedDurably,
            AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } => {
                UserConfigPublication::PublishedDurabilityUncertain { diagnostic }
            }
        })
    }
}

fn apply(
    document: &mut DocumentMut,
    parsed: &ConfigLayer,
    mutation: UserConfigMutation,
) -> Result<(), UserConfigError> {
    match mutation {
        UserConfigMutation::ModelAndThinking { model, thinking } => {
            let root = document.as_table_mut();
            replace_string(root.entry("model").or_insert(Item::None), &model)?;
            replace_string(
                root.entry("thinking").or_insert(Item::None),
                thinking.as_str(),
            )?;
        }
        UserConfigMutation::Statusline { segments } => {
            let mut array = Array::new();
            for segment in segments {
                array.push(segment);
            }
            let ui = document.as_table_mut().entry("ui").or_insert(Item::None);
            let statusline = ui.get_mut("statusline").ok_or_else(|| {
                UserConfigError::new("user config `ui.statusline` cannot contain the segment list")
            })?;
            let target = statusline.get_mut("segments").ok_or_else(|| {
                UserConfigError::new(
                    "user config `ui.statusline.segments` is not a writable table field",
                )
            })?;
            replace_item_preserving_decor(target, value(array));
        }
        UserConfigMutation::McpSet {
            slot,
            name,
            expected_user_entry,
            server,
        } => {
            let current = mcp_entry(parsed, &slot, &name);
            if current != expected_user_entry.as_ref() {
                return Err(UserConfigError::new(
                    "user config changed before the MCP entry could be set",
                ));
            }
            let target = mcp_servers_table_mut(document, &slot)?;
            target.insert(&name.to_string(), Item::Table(server_table(&server)?));
        }
        UserConfigMutation::McpDeleteUserEntry {
            slot,
            name,
            expected_entry,
        } => {
            let current = mcp_entry(parsed, &slot, &name);
            if current != Some(&expected_entry) {
                return Err(UserConfigError::new(
                    "user config changed before the MCP entry could be deleted",
                ));
            }
            let target = existing_mcp_servers_table_mut(document, &slot).ok_or_else(|| {
                UserConfigError::new("the expected MCP entry is absent from the user config")
            })?;
            if target.remove(&name.to_string()).is_none() {
                return Err(UserConfigError::new(
                    "the expected MCP entry is absent from the user config",
                ));
            }
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
            let target = mcp_servers_table_mut(document, &UserMcpSlot::Base)?;
            let mut tombstone = Table::new();
            tombstone.insert("removed", value(true));
            target.insert(&name.to_string(), Item::Table(tombstone));
        }
    }
    Ok(())
}

fn mcp_entry<'a>(
    parsed: &'a ConfigLayer,
    slot: &UserMcpSlot,
    name: &ego_core::config::McpServerName,
) -> Option<&'a McpLayerEntry> {
    match slot {
        UserMcpSlot::Base => parsed.mcp.servers.get(name),
        UserMcpSlot::Profile(profile) => parsed
            .profile
            .get(profile)
            .and_then(|layer| layer.mcp.servers.get(name)),
    }
}

fn mcp_servers_table_mut<'a>(
    document: &'a mut DocumentMut,
    slot: &UserMcpSlot,
) -> Result<&'a mut Table, UserConfigError> {
    let root = document.as_table_mut();
    let mcp = match slot {
        UserMcpSlot::Base => table_field_mut(root, "mcp")?,
        UserMcpSlot::Profile(profile) => {
            let profiles = table_field_mut(root, "profile")?;
            let profile = table_field_mut(profiles, profile)?;
            table_field_mut(profile, "mcp")?
        }
    };
    table_field_mut(mcp, "servers")
}

fn existing_mcp_servers_table_mut<'a>(
    document: &'a mut DocumentMut,
    slot: &UserMcpSlot,
) -> Option<&'a mut Table> {
    let root = document.as_table_mut();
    let mcp = match slot {
        UserMcpSlot::Base => root.get_mut("mcp")?.as_table_mut()?,
        UserMcpSlot::Profile(profile) => root
            .get_mut("profile")?
            .as_table_mut()?
            .get_mut(profile)?
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
