//! OS adapter for the atomic user-configuration mutation port.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use ego_core::config::{
    ConfigLayer, Source, UserConfigError, UserConfigMutation, UserConfigPublication,
    UserConfigStore,
};
use fd_lock::RwLock;
use toml_edit::{Array, DocumentMut, Item, Value, value};

use crate::store::{AtomicReplacePublication, StoreIo};

/// The real filesystem adapter for one user `config.toml` authority.
pub struct OsUserConfigStore<'io, I> {
    io: &'io I,
    path: PathBuf,
}

impl<'io, I> OsUserConfigStore<'io, I> {
    pub fn new(io: &'io I, path: impl AsRef<Path>) -> Self {
        Self {
            io,
            path: path.as_ref().to_path_buf(),
        }
    }
}

impl<I: StoreIo> UserConfigStore for OsUserConfigStore<'_, I> {
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
            Some(bytes) => String::from_utf8(bytes)
                .map_err(|error| UserConfigError::new(format!("user config is not UTF-8: {error}")))?,
            None => String::new(),
        };
        let mut document = text
            .parse::<DocumentMut>()
            .map_err(|error| UserConfigError::new(format!("invalid user config TOML: {error}")))?;

        apply(&mut document, mutation)?;
        let published = document.to_string();
        ConfigLayer::parse(Source::User, &published).map_err(|error| {
            UserConfigError::new(format!("mutated user config failed strict validation: {error}"))
        })?;
        let publication = self
            .io
            .replace_atomic_publication(&self.path, published.as_bytes())
            .map_err(transaction_error)?;
        Ok(match publication {
            AtomicReplacePublication::PublishedDurably => {
                UserConfigPublication::PublishedDurably
            }
            AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } => {
                UserConfigPublication::PublishedDurabilityUncertain { diagnostic }
            }
        })
    }
}

fn apply(
    document: &mut DocumentMut,
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
            let ui = document
                .as_table_mut()
                .entry("ui")
                .or_insert(Item::None);
            let statusline = ui.get_mut("statusline").ok_or_else(|| {
                UserConfigError::new(
                    "user config `ui.statusline` cannot contain the segment list",
                )
            })?;
            let target = statusline.get_mut("segments").ok_or_else(|| {
                UserConfigError::new(
                    "user config `ui.statusline.segments` is not a writable table field",
                )
            })?;
            replace_item_preserving_decor(target, value(array));
        }
    }
    Ok(())
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
