//! ACP bearer authentication for credential-requiring inbound endpoints.

use std::fmt;

use ego_core::credentials::SecretMaterial;
use ego_core::endpoint::BearerCredential;

/// The environment variable holding ACP's network bearer secret.
pub const TOKEN_VAR: &str = "EGO_ACP_TOKEN";

/// One loaded ACP bearer credential.
pub struct Credential {
    bearer: BearerCredential,
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Credential").finish_non_exhaustive()
    }
}

/// Why ACP could not establish its network credential at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialRefusal {
    Unset,
    Empty,
}

impl fmt::Display for CredentialRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unset => "the ACP bearer credential is not configured: set EGO_ACP_TOKEN",
            Self::Empty => "EGO_ACP_TOKEN is empty; set it to a non-empty secret",
        })
    }
}

impl std::error::Error for CredentialRefusal {}

impl Credential {
    /// Load the protocol-specific token without retaining its source string.
    pub fn from_env(read: impl FnOnce(&str) -> Option<String>) -> Result<Self, CredentialRefusal> {
        let token = read(TOKEN_VAR).ok_or(CredentialRefusal::Unset)?;
        if token.is_empty() {
            return Err(CredentialRefusal::Empty);
        }
        Ok(Self {
            bearer: BearerCredential::new(SecretMaterial::new(token.into_bytes())),
        })
    }

    /// Match one complete HTTP `Authorization` value.
    pub fn admits(&self, authorization: &[u8]) -> bool {
        let Some(presented) = strip_bearer(authorization) else {
            return false;
        };
        self.bearer.matches(presented)
    }
}

fn strip_bearer(authorization: &[u8]) -> Option<&[u8]> {
    let (scheme, remainder) = split_first_space(authorization)?;
    if !scheme.eq_ignore_ascii_case(b"bearer") {
        return None;
    }
    Some(trim_leading_spaces(remainder))
}

fn split_first_space(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let separator = bytes.iter().position(|byte| *byte == b' ')?;
    Some((&bytes[..separator], &bytes[separator + 1..]))
}

fn trim_leading_spaces(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| *byte != b' ')
        .unwrap_or(bytes.len());
    &bytes[start..]
}
