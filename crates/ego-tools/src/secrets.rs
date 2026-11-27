//! Environment-owned secret inventory; never reads credential stores.

use std::ffi::OsString;

use ego_core::secrets::SecretValues;

/// Values shorter than this many bytes are never masked: they would corrupt ordinary output.
pub const MIN_SECRET_LEN: usize = ego_core::secrets::MIN_ENV_SECRET_LEN;

pub fn from_env() -> SecretValues {
    from_vars(std::env::vars_os())
}

/// Inventory of the given variables, with the same policy as [`from_env`].
pub fn from_vars(vars: impl IntoIterator<Item = (OsString, OsString)>) -> SecretValues {
    SecretValues::new(vars.into_iter().filter_map(|(name, value)| {
        if ego_core::secrets::is_secret_environment_name(&name.to_string_lossy()) {
            value
                .into_string()
                .ok()
                .filter(|value| value.len() >= MIN_SECRET_LEN)
                .map(String::into_bytes)
        } else {
            None
        }
    }))
}
