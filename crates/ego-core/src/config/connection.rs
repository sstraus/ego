//! User-owned account identity, independent of names and protocol routes.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionAccess {
    Key,
    Oauth,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionConfig {
    pub name: String,
    pub vendor: String,
    pub access: ConnectionAccess,
    pub key_env: Option<String>,
    /// Exact admitted Azure OpenAI v1 resource for this account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub azure_resource: Option<super::ProviderEndpoint>,
}

/// Durable provider-local number assignments, including retired accounts.
pub type AccountNumbers = BTreeMap<String, BTreeMap<String, u32>>;

pub(crate) fn validate_numbers(numbers: &AccountNumbers) -> Result<(), String> {
    for (vendor, assignments) in numbers {
        let mut seen = std::collections::BTreeSet::new();
        if !identifier(vendor)
            || assignments
                .iter()
                .any(|(id, number)| !identifier(id) || *number == 0 || !seen.insert(*number))
        {
            return Err(format!("invalid account number assignments for {vendor}"));
        }
    }
    Ok(())
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

impl ConnectionConfig {
    pub fn validate(&self, id: &str) -> Result<(), String> {
        if !identifier(id) || !identifier(&self.vendor) {
            return Err(
                "connection ID and vendor require letters, digits, underscores or hyphens".into(),
            );
        }
        if self.name.trim().is_empty()
            || self.name.chars().count() > 128
            || self.name.chars().any(char::is_control)
        {
            return Err(
                "connection name requires 1..128 characters without control characters".into(),
            );
        }
        match (self.vendor.as_str(), &self.azure_resource) {
            (super::AZURE_OPENAI, Some(resource)) if resource.is_azure_openai_v1() => {
                if self.access == ConnectionAccess::Key && self.key_env.is_none() {
                    return Err("azure-openai key connections require a scoped key_env".into());
                }
            }
            (super::AZURE_OPENAI, _) => {
                return Err("azure-openai connections require azure_resource with the exact public Azure /openai/v1/ base".into());
            }
            (_, Some(_)) => return Err("azure_resource requires the azure-openai vendor".into()),
            _ => {}
        }
        match (self.access, self.key_env.as_deref()) {
            (ConnectionAccess::Oauth | ConnectionAccess::Key, None) => Ok(()),
            (ConnectionAccess::Key, Some(name))
                if !name.is_empty()
                    && name.bytes().enumerate().all(|(index, byte)| {
                        byte == b'_'
                            || byte.is_ascii_alphabetic()
                            || (index > 0 && byte.is_ascii_digit())
                    }) =>
            {
                if !crate::secrets::is_secret_environment_name(name) {
                    return Err("key_env requires a secret-marked name such as WORK_API_KEY so tool output masks its value".into());
                }
                Ok(())
            }
            _ => Err(
                "key_env must name a secret environment variable; OAuth connections forbid key_env"
                    .into(),
            ),
        }
    }
}

/// Select by stable ID only. Labels and map iteration never decide an account.
pub fn select_connection<'a>(
    connections: &'a BTreeMap<String, ConnectionConfig>,
    selected: Option<&str>,
    vendor: Option<&str>,
) -> Result<Option<(&'a str, &'a ConnectionConfig)>, String> {
    if let Some(id) = selected {
        let (id, connection) = connections.get_key_value(id).ok_or_else(|| {
            "unknown connection: the selected account is no longer saved".to_owned()
        })?;
        if let Some(vendor) = vendor
            && vendor != connection.vendor
        {
            return Err(format!(
                "selected account belongs to {}, not {vendor}",
                connection.vendor
            ));
        }
        return Ok(Some((id, connection)));
    }
    let mut matches = connections
        .iter()
        .filter(|(_, connection)| vendor.is_none_or(|vendor| vendor == connection.vendor));
    let first = matches.next();
    if matches.next().is_some() {
        return Err(match vendor {
            Some(vendor) => format!(
                "multiple {vendor} accounts match; run 'ego auth status {vendor}' and select --account N"
            ),
            None => {
                "multiple accounts match; specify the provider with --model PROVIDER/MODEL".into()
            }
        });
    }
    Ok(first.map(|(id, connection)| (id.as_str(), connection)))
}
