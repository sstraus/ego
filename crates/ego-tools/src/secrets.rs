//! Environment-owned secret inventory; never reads credential stores.

use std::ffi::OsString;

use ego_core::secrets::SecretValues;

/// Values shorter than this many bytes are never masked: they would corrupt ordinary output.
pub const MIN_SECRET_LEN: usize = 8;

/// A name segment (split on `_`, case-insensitive) that ends with one of these words
/// marks a variable as secret (`API_KEY`, `PGPASSWORD`, but not `MY_KEYBOARD`).
const SECRET_SEGMENTS: [&str; 4] = ["TOKEN", "KEY", "SECRET", "PASSWORD"];

pub fn from_env() -> SecretValues {
    from_vars(std::env::vars_os())
}

/// Inventory of the given variables, with the same policy as [`from_env`].
pub fn from_vars(vars: impl IntoIterator<Item = (OsString, OsString)>) -> SecretValues {
    SecretValues::new(vars.into_iter().filter_map(|(name, value)| {
        let name = name.to_string_lossy().to_ascii_uppercase();
        let sensitive = name
            .split('_')
            .any(|seg| SECRET_SEGMENTS.iter().any(|word| seg.ends_with(word)))
            || name == "AWS_ACCESS_KEY_ID"
            || name == "GOOGLE_APPLICATION_CREDENTIALS";
        if sensitive {
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
