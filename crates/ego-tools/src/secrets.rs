//! Environment-owned secret inventory; never reads credential stores.

use ego_core::secrets::SecretValues;

pub fn from_env() -> SecretValues {
    SecretValues::new(std::env::vars_os().filter_map(|(name, value)| {
        let name = name.to_string_lossy().to_ascii_uppercase();
        let sensitive = ["TOKEN", "KEY", "SECRET", "PASSWORD"]
            .iter()
            .any(|part| name.contains(part))
            || name == "AWS_ACCESS_KEY_ID"
            || name == "GOOGLE_APPLICATION_CREDENTIALS";
        if sensitive {
            value.into_string().ok().map(String::into_bytes)
        } else {
            None
        }
    }))
}
