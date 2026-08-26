//! Consumer-owned credential storage boundary for MCP authentication.
//!
//! Core defines identity, secret ownership, and closed errors. Native keyring
//! selection and the stored envelope remain adapter responsibilities.

use zeroize::Zeroizing;

/// The authority target a credential is allowed to serve.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CredentialTarget {
    ProtectedResource(String),
    Audience(String),
}

/// Issuer, target domain, and optional issuer account identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialKey {
    issuer: String,
    target: CredentialTarget,
    account: Option<String>,
}

impl CredentialKey {
    pub fn new(
        issuer: String,
        target: CredentialTarget,
        account: Option<String>,
    ) -> Result<Self, CredentialKeyError> {
        reject_empty("issuer", &issuer)?;
        match &target {
            CredentialTarget::ProtectedResource(value) => {
                reject_empty("protected resource", value)?;
            }
            CredentialTarget::Audience(value) => reject_empty("audience", value)?,
        }
        if let Some(account) = &account {
            reject_empty("account", account)?;
        }
        Ok(Self {
            issuer,
            target,
            account,
        })
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn target(&self) -> &CredentialTarget {
        &self.target
    }

    pub fn account(&self) -> Option<&str> {
        self.account.as_deref()
    }

    /// Stable domain-tagged, length-prefixed bytes for hashing and envelope
    /// collision checks. Resource and audience use different field tags.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        push_field(&mut bytes, 0, b"ego:mcp:credential-key:v1");
        push_field(&mut bytes, 1, self.issuer.as_bytes());
        match &self.target {
            CredentialTarget::ProtectedResource(value) => {
                push_field(&mut bytes, 2, value.as_bytes());
            }
            CredentialTarget::Audience(value) => {
                push_field(&mut bytes, 3, value.as_bytes());
            }
        }
        match &self.account {
            None => push_field(&mut bytes, 4, &[0]),
            Some(account) => {
                push_field(&mut bytes, 4, &[1]);
                push_field(&mut bytes, 5, account.as_bytes());
            }
        }
        bytes
    }
}

fn push_field(bytes: &mut Vec<u8>, tag: u8, value: &[u8]) {
    bytes.push(tag);
    bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
    bytes.extend_from_slice(value);
}

fn reject_empty(field: &'static str, value: &str) -> Result<(), CredentialKeyError> {
    if value.trim().is_empty() {
        Err(CredentialKeyError { field })
    } else {
        Ok(())
    }
}

/// A canonical credential identity omitted a required value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialKeyError {
    field: &'static str,
}

impl std::fmt::Display for CredentialKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "credential {field} must not be empty",
            field = self.field
        )
    }
}

impl std::error::Error for CredentialKeyError {}

/// Owned opaque bytes that are zeroized on drop.
pub struct SecretMaterial {
    bytes: Zeroizing<Vec<u8>>,
}

impl SecretMaterial {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Zeroizing::new(bytes),
        }
    }

    /// Explicitly expose secret bytes to the protocol owner or store adapter.
    pub fn expose_secret_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }
}

impl std::fmt::Debug for SecretMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretMaterial([REDACTED])")
    }
}

/// Result of deleting one credential binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialRemoval {
    Removed,
    Absent,
}

/// Store operation that failed without leaking an outer adapter error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStoreOperation {
    Get,
    Put,
    Delete,
}

impl std::fmt::Display for CredentialStoreOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Get => "get",
            Self::Put => "put",
            Self::Delete => "delete",
        })
    }
}

/// Closed, non-secret classification of a credential-store failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStoreErrorKind {
    Unavailable,
    Invalid,
    Ambiguous,
    Corrupt,
    Unsupported,
    UnsupportedVersion,
    Collision,
}

impl std::fmt::Display for CredentialStoreErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "unavailable",
            Self::Invalid => "invalid",
            Self::Ambiguous => "ambiguous",
            Self::Corrupt => "corrupt",
            Self::Unsupported => "unsupported",
            Self::UnsupportedVersion => "unsupported version",
            Self::Collision => "collision",
        })
    }
}

/// Sanitized core error; raw keyring errors and attached data never cross it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialStoreError {
    operation: CredentialStoreOperation,
    kind: CredentialStoreErrorKind,
}

impl CredentialStoreError {
    pub fn new(operation: CredentialStoreOperation, kind: CredentialStoreErrorKind) -> Self {
        Self { operation, kind }
    }

    pub fn operation(&self) -> CredentialStoreOperation {
        self.operation
    }

    pub fn kind(&self) -> CredentialStoreErrorKind {
        self.kind
    }

    pub fn diagnostic(&self) -> &'static str {
        match self.kind {
            CredentialStoreErrorKind::Unavailable => "credential store is unavailable",
            CredentialStoreErrorKind::Invalid => "credential store request is invalid",
            CredentialStoreErrorKind::Ambiguous => "credential store result is ambiguous",
            CredentialStoreErrorKind::Corrupt => "stored credential is corrupt",
            CredentialStoreErrorKind::Unsupported => "credential store operation is unsupported",
            CredentialStoreErrorKind::UnsupportedVersion => {
                "stored credential version is unsupported"
            }
            CredentialStoreErrorKind::Collision => "stored credential key collision",
        }
    }
}

impl std::fmt::Display for CredentialStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "credential store {operation} failed ({kind}): {diagnostic}",
            operation = self.operation,
            kind = self.kind,
            diagnostic = self.diagnostic(),
        )
    }
}

impl std::error::Error for CredentialStoreError {}

/// Synchronous consumer port for one issuer/target/account-bound secret.
pub trait CredentialStore: Send + Sync {
    fn get(&self, key: &CredentialKey) -> Result<Option<SecretMaterial>, CredentialStoreError>;

    fn put(&self, key: &CredentialKey, value: SecretMaterial) -> Result<(), CredentialStoreError>;

    fn delete(&self, key: &CredentialKey) -> Result<CredentialRemoval, CredentialStoreError>;
}
