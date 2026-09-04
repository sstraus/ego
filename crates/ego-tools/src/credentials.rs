//! Native OS adapter for the core-owned credential-store port.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ego_core::credentials::{
    CredentialKey, CredentialRemoval, CredentialStore, CredentialStoreError,
    CredentialStoreErrorKind, CredentialStoreOperation, SecretMaterial,
};
use ego_core::suspension::{ContinuationError, ResumeKeySource};
use ego_core::turn::{
    GenerationPolicyCaptureStore, GenerationPolicyCaptureStoreError,
    GenerationPolicyCaptureStoreErrorKind, GenerationPolicyCaptureStoreOperation,
    GenerationPolicyDigest, GenerationPolicySnapshot,
};
use fd_lock::RwLock;
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

const SERVICE: &str = "ego";
const OAUTH_USER_PREFIX: &str = "mcp/oauth/v1/";
const OAUTH_ENVELOPE_MAGIC: &[u8; 8] = b"EGO-MCP\0";
const OAUTH_ENVELOPE_VERSION: u8 = 1;
const CAPTURE_USER_PREFIX: &str = "mcp/generation-policy-capture/v1/";
const CAPTURE_KEY_DOMAIN: &[u8] = b"ego:generation-policy-capture-key:v1";
const CAPTURE_ENVELOPE_MAGIC: &[u8; 8] = b"EGO-GPC\0";
const CAPTURE_ENVELOPE_VERSION: u8 = 1;
const RESUME_USER: &str = "mcp/resume/v1";
const RESUME_LOCK: &str = "mcp-resume-key.lock";

/// The closed failure surface exposed by a native credential backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialBackendError {
    NoEntry,
    Invalid,
    TooLong,
    Ambiguous,
    BadEncoding,
    BadDataFormat,
    BadStoreFormat,
    NotSupportedByStore,
    PlatformFailure,
    NoStorageAccess,
    NoDefaultStore,
    /// An unknown or future backend failure.
    Unavailable,
}

impl std::fmt::Display for CredentialBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoEntry => "native credential entry absent",
            Self::Invalid | Self::TooLong => "native credential request invalid",
            Self::Ambiguous => "native credential entry ambiguous",
            Self::BadEncoding | Self::BadDataFormat | Self::BadStoreFormat => {
                "native credential data corrupt"
            }
            Self::NotSupportedByStore => "native credential operation unsupported",
            Self::PlatformFailure
            | Self::NoStorageAccess
            | Self::NoDefaultStore
            | Self::Unavailable => "native credential store unavailable",
        })
    }
}

impl std::error::Error for CredentialBackendError {}

/// Minimal binary-secret operations required by the credential adapter.
pub trait CredentialBackend: Send + Sync {
    fn get(&self, service: &str, user: &str) -> Result<Option<Vec<u8>>, CredentialBackendError>;
    fn put(&self, service: &str, user: &str, value: Vec<u8>) -> Result<(), CredentialBackendError>;
    fn delete(&self, service: &str, user: &str) -> Result<bool, CredentialBackendError>;
}

/// Platform-selected `keyring` v1 binary-secret backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeKeyringBackend;

impl CredentialBackend for NativeKeyringBackend {
    fn get(&self, service: &str, user: &str) -> Result<Option<Vec<u8>>, CredentialBackendError> {
        let entry = keyring::Entry::new(service, user).map_err(map_keyring_error)?;
        match entry.get_secret() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(map_keyring_error(error)),
        }
    }

    fn put(
        &self,
        service: &str,
        user: &str,
        mut value: Vec<u8>,
    ) -> Result<(), CredentialBackendError> {
        let result = match keyring::Entry::new(service, user) {
            Ok(entry) => entry.set_secret(&value),
            Err(error) => Err(error),
        };
        value.zeroize();
        result.map_err(map_keyring_error)
    }

    fn delete(&self, service: &str, user: &str) -> Result<bool, CredentialBackendError> {
        let entry = keyring::Entry::new(service, user).map_err(map_keyring_error)?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(map_keyring_error(error)),
        }
    }
}

fn map_keyring_error(error: keyring::Error) -> CredentialBackendError {
    match error {
        keyring::Error::NoEntry => CredentialBackendError::NoEntry,
        keyring::Error::Invalid(_, _) => CredentialBackendError::Invalid,
        keyring::Error::TooLong(_, _) => CredentialBackendError::TooLong,
        keyring::Error::Ambiguous(_) => CredentialBackendError::Ambiguous,
        keyring::Error::BadEncoding(mut bytes) => {
            bytes.zeroize();
            CredentialBackendError::BadEncoding
        }
        keyring::Error::BadDataFormat(mut bytes, _) => {
            bytes.zeroize();
            CredentialBackendError::BadDataFormat
        }
        keyring::Error::BadStoreFormat(mut format) => {
            format.zeroize();
            CredentialBackendError::BadStoreFormat
        }
        keyring::Error::NotSupportedByStore(_) => CredentialBackendError::NotSupportedByStore,
        keyring::Error::PlatformFailure(_) => CredentialBackendError::PlatformFailure,
        keyring::Error::NoStorageAccess(_) => CredentialBackendError::NoStorageAccess,
        keyring::Error::NoDefaultStore => CredentialBackendError::NoDefaultStore,
        _ => CredentialBackendError::Unavailable,
    }
}

/// One-entry-per-binding adapter over an injected credential backend.
pub struct OsCredentialStore<B = NativeKeyringBackend> {
    backend: B,
}

impl<B> OsCredentialStore<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}

impl Default for OsCredentialStore<NativeKeyringBackend> {
    fn default() -> Self {
        Self::new(NativeKeyringBackend)
    }
}

/// Native confidential storage for generation policy captures.
pub struct OsGenerationPolicyCaptureStore<B = NativeKeyringBackend> {
    backend: B,
}

impl<B> OsGenerationPolicyCaptureStore<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}

impl Default for OsGenerationPolicyCaptureStore<NativeKeyringBackend> {
    fn default() -> Self {
        Self::new(NativeKeyringBackend)
    }
}

/// Stable machine resume key stored in the native credential backend.
pub struct OsResumeKey<B = NativeKeyringBackend> {
    backend: B,
    lock_path: PathBuf,
}

impl OsResumeKey<NativeKeyringBackend> {
    pub fn open(root: impl AsRef<Path>) -> Self {
        Self::new(NativeKeyringBackend, root.as_ref().join(RESUME_LOCK))
    }
}

impl<B> OsResumeKey<B> {
    pub fn new(backend: B, lock_path: impl Into<PathBuf>) -> Self {
        Self {
            backend,
            lock_path: lock_path.into(),
        }
    }
}

impl<B> ResumeKeySource for OsResumeKey<B>
where
    B: CredentialBackend,
{
    fn resume_key(&self) -> Result<Arc<[u8]>, ContinuationError> {
        if let Some(parent) = self.lock_path.parent() {
            fs::create_dir_all(parent).map_err(|_| resume_key_unavailable())?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|_| resume_key_unavailable())?;
        let mut lock = RwLock::new(file);
        let _guard = lock.write().map_err(|_| resume_key_unavailable())?;
        match self
            .backend
            .get(SERVICE, RESUME_USER)
            .map_err(|_| resume_key_unavailable())?
        {
            Some(key) => resume_key_bytes(key),
            None => {
                let mut key = [0_u8; 32];
                getrandom::fill(&mut key).map_err(|_| resume_key_unavailable())?;
                self.backend
                    .put(SERVICE, RESUME_USER, key.to_vec())
                    .map_err(|_| resume_key_unavailable())?;
                let stored = self
                    .backend
                    .get(SERVICE, RESUME_USER)
                    .map_err(|_| resume_key_unavailable())?
                    .ok_or_else(resume_key_unavailable)?;
                key.zeroize();
                resume_key_bytes(stored)
            }
        }
    }
}

fn resume_key_bytes(mut key: Vec<u8>) -> Result<Arc<[u8]>, ContinuationError> {
    if key.len() != 32 {
        key.zeroize();
        return Err(ContinuationError::Corrupt(
            "native resume key has invalid length".to_owned(),
        ));
    }
    let shared: Arc<[u8]> = Arc::from(key.as_slice());
    key.zeroize();
    Ok(shared)
}

fn resume_key_unavailable() -> ContinuationError {
    ContinuationError::Unavailable("native resume key is unavailable".to_owned())
}

impl<B> GenerationPolicyCaptureStore for OsGenerationPolicyCaptureStore<B>
where
    B: CredentialBackend,
{
    fn store(
        &self,
        id: &str,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<(), GenerationPolicyCaptureStoreError> {
        let canonical_key = Zeroizing::new(capture_key(id));
        let user = user_for(CAPTURE_USER_PREFIX, &canonical_key);
        match self.backend.get(SERVICE, &user) {
            Ok(Some(existing)) => {
                let existing = Zeroizing::new(existing);
                decode_capture_snapshot(&canonical_key, &existing).map_err(|kind| {
                    capture_error(GenerationPolicyCaptureStoreOperation::Store, kind)
                })?;
            }
            Ok(None) | Err(CredentialBackendError::NoEntry) => {}
            Err(error) => {
                return Err(capture_backend_error(
                    GenerationPolicyCaptureStoreOperation::Store,
                    error,
                    false,
                ));
            }
        }

        let confidential = snapshot.encode_confidential();
        let envelope = encode_envelope(
            CAPTURE_ENVELOPE_MAGIC,
            CAPTURE_ENVELOPE_VERSION,
            &canonical_key,
            confidential.expose_secret_bytes(),
        )
        .ok_or_else(|| {
            capture_error(
                GenerationPolicyCaptureStoreOperation::Store,
                GenerationPolicyCaptureStoreErrorKind::CapacityExceeded,
            )
        })?;
        self.backend.put(SERVICE, &user, envelope).map_err(|error| {
            capture_backend_error(GenerationPolicyCaptureStoreOperation::Store, error, true)
        })
    }

    fn load(
        &self,
        id: &str,
        expected: &GenerationPolicyDigest,
    ) -> Result<Arc<GenerationPolicySnapshot>, GenerationPolicyCaptureStoreError> {
        let canonical_key = Zeroizing::new(capture_key(id));
        let user = user_for(CAPTURE_USER_PREFIX, &canonical_key);
        let envelope = match self.backend.get(SERVICE, &user) {
            Ok(Some(envelope)) => envelope,
            Ok(None) | Err(CredentialBackendError::NoEntry) => {
                return Err(capture_error(
                    GenerationPolicyCaptureStoreOperation::Load,
                    GenerationPolicyCaptureStoreErrorKind::Missing,
                ));
            }
            Err(error) => {
                return Err(capture_backend_error(
                    GenerationPolicyCaptureStoreOperation::Load,
                    error,
                    false,
                ));
            }
        };
        let envelope = Zeroizing::new(envelope);
        let snapshot = Arc::new(
            decode_capture_snapshot(&canonical_key, &envelope)
                .map_err(|kind| capture_error(GenerationPolicyCaptureStoreOperation::Load, kind))?,
        );
        if snapshot.digest() != expected.clone() {
            return Err(capture_error(
                GenerationPolicyCaptureStoreOperation::Load,
                GenerationPolicyCaptureStoreErrorKind::DigestMismatch,
            ));
        }
        Ok(snapshot)
    }

    fn delete(&self, id: &str) -> Result<(), GenerationPolicyCaptureStoreError> {
        let canonical_key = Zeroizing::new(capture_key(id));
        let user = user_for(CAPTURE_USER_PREFIX, &canonical_key);
        let envelope = match self.backend.get(SERVICE, &user) {
            Ok(Some(envelope)) => envelope,
            Ok(None) | Err(CredentialBackendError::NoEntry) => return Ok(()),
            Err(error) => {
                return Err(capture_backend_error(
                    GenerationPolicyCaptureStoreOperation::Delete,
                    error,
                    false,
                ));
            }
        };
        let envelope = Zeroizing::new(envelope);
        decode_capture_snapshot(&canonical_key, &envelope)
            .map_err(|kind| capture_error(GenerationPolicyCaptureStoreOperation::Delete, kind))?;
        match self.backend.delete(SERVICE, &user) {
            Ok(_) | Err(CredentialBackendError::NoEntry) => Ok(()),
            Err(error) => Err(capture_backend_error(
                GenerationPolicyCaptureStoreOperation::Delete,
                error,
                false,
            )),
        }
    }
}

impl<B> CredentialStore for OsCredentialStore<B>
where
    B: CredentialBackend,
{
    fn get(&self, key: &CredentialKey) -> Result<Option<SecretMaterial>, CredentialStoreError> {
        let canonical_key = key.canonical_bytes();
        let user = user_for(OAUTH_USER_PREFIX, &canonical_key);
        let Some(envelope) = self
            .backend
            .get(SERVICE, &user)
            .or_else(|error| match error {
                CredentialBackendError::NoEntry => Ok(None),
                error => Err(backend_error(CredentialStoreOperation::Get, error)),
            })?
        else {
            return Ok(None);
        };
        let envelope = Zeroizing::new(envelope);
        let secret = decode_envelope(
            OAUTH_ENVELOPE_MAGIC,
            OAUTH_ENVELOPE_VERSION,
            &canonical_key,
            &envelope,
        )
        .map_err(|error| envelope_error(CredentialStoreOperation::Get, error.credential_kind()))?;
        Ok(Some(SecretMaterial::new(secret.to_vec())))
    }

    fn put(&self, key: &CredentialKey, value: SecretMaterial) -> Result<(), CredentialStoreError> {
        let canonical_key = key.canonical_bytes();
        let user = user_for(OAUTH_USER_PREFIX, &canonical_key);
        if let Some(existing) = self
            .backend
            .get(SERVICE, &user)
            .map_err(|error| backend_error(CredentialStoreOperation::Put, error))?
        {
            let existing = Zeroizing::new(existing);
            decode_envelope(
                OAUTH_ENVELOPE_MAGIC,
                OAUTH_ENVELOPE_VERSION,
                &canonical_key,
                &existing,
            )
            .map_err(|error| {
                envelope_error(CredentialStoreOperation::Put, error.credential_kind())
            })?;
        }
        let envelope = encode_envelope(
            OAUTH_ENVELOPE_MAGIC,
            OAUTH_ENVELOPE_VERSION,
            &canonical_key,
            value.expose_secret_bytes(),
        )
        .ok_or_else(|| {
            CredentialStoreError::new(
                CredentialStoreOperation::Put,
                CredentialStoreErrorKind::Invalid,
            )
        })?;
        self.backend
            .put(SERVICE, &user, envelope)
            .map_err(|error| backend_error(CredentialStoreOperation::Put, error))
    }

    fn delete(&self, key: &CredentialKey) -> Result<CredentialRemoval, CredentialStoreError> {
        let canonical_key = key.canonical_bytes();
        let user = user_for(OAUTH_USER_PREFIX, &canonical_key);
        let Some(envelope) = self
            .backend
            .get(SERVICE, &user)
            .or_else(|error| match error {
                CredentialBackendError::NoEntry => Ok(None),
                error => Err(backend_error(CredentialStoreOperation::Delete, error)),
            })?
        else {
            return Ok(CredentialRemoval::Absent);
        };
        let envelope = Zeroizing::new(envelope);
        decode_envelope(
            OAUTH_ENVELOPE_MAGIC,
            OAUTH_ENVELOPE_VERSION,
            &canonical_key,
            &envelope,
        )
        .map_err(|error| {
            envelope_error(CredentialStoreOperation::Delete, error.credential_kind())
        })?;
        self.backend.delete(SERVICE, &user).map_or_else(
            |error| match error {
                CredentialBackendError::NoEntry => Ok(CredentialRemoval::Absent),
                error => Err(backend_error(CredentialStoreOperation::Delete, error)),
            },
            |removed| {
                Ok(if removed {
                    CredentialRemoval::Removed
                } else {
                    CredentialRemoval::Absent
                })
            },
        )
    }
}

fn backend_error(
    operation: CredentialStoreOperation,
    error: CredentialBackendError,
) -> CredentialStoreError {
    let kind = match error {
        CredentialBackendError::Invalid | CredentialBackendError::TooLong => {
            CredentialStoreErrorKind::Invalid
        }
        CredentialBackendError::Ambiguous => CredentialStoreErrorKind::Ambiguous,
        CredentialBackendError::BadEncoding
        | CredentialBackendError::BadDataFormat
        | CredentialBackendError::BadStoreFormat => CredentialStoreErrorKind::Corrupt,
        CredentialBackendError::NotSupportedByStore => CredentialStoreErrorKind::Unsupported,
        CredentialBackendError::NoEntry
        | CredentialBackendError::PlatformFailure
        | CredentialBackendError::NoStorageAccess
        | CredentialBackendError::NoDefaultStore
        | CredentialBackendError::Unavailable => CredentialStoreErrorKind::Unavailable,
    };
    CredentialStoreError::new(operation, kind)
}

fn envelope_error(
    operation: CredentialStoreOperation,
    kind: CredentialStoreErrorKind,
) -> CredentialStoreError {
    CredentialStoreError::new(operation, kind)
}

fn capture_backend_error(
    operation: GenerationPolicyCaptureStoreOperation,
    error: CredentialBackendError,
    capacity_on_too_long: bool,
) -> GenerationPolicyCaptureStoreError {
    let kind = match error {
        CredentialBackendError::TooLong if capacity_on_too_long => {
            GenerationPolicyCaptureStoreErrorKind::CapacityExceeded
        }
        CredentialBackendError::BadEncoding
        | CredentialBackendError::BadDataFormat
        | CredentialBackendError::BadStoreFormat => GenerationPolicyCaptureStoreErrorKind::Corrupt,
        CredentialBackendError::NoEntry
        | CredentialBackendError::Invalid
        | CredentialBackendError::TooLong
        | CredentialBackendError::Ambiguous
        | CredentialBackendError::NotSupportedByStore
        | CredentialBackendError::PlatformFailure
        | CredentialBackendError::NoStorageAccess
        | CredentialBackendError::NoDefaultStore
        | CredentialBackendError::Unavailable => GenerationPolicyCaptureStoreErrorKind::Unavailable,
    };
    capture_error(operation, kind)
}

fn capture_error(
    operation: GenerationPolicyCaptureStoreOperation,
    kind: GenerationPolicyCaptureStoreErrorKind,
) -> GenerationPolicyCaptureStoreError {
    GenerationPolicyCaptureStoreError::new(operation, kind)
}

fn capture_key(id: &str) -> Vec<u8> {
    let mut key = Vec::new();
    push_capture_key_field(&mut key, 0, CAPTURE_KEY_DOMAIN);
    push_capture_key_field(&mut key, 1, id.as_bytes());
    key
}

fn push_capture_key_field(key: &mut Vec<u8>, tag: u8, value: &[u8]) {
    key.push(tag);
    key.extend_from_slice(&(value.len() as u64).to_be_bytes());
    key.extend_from_slice(value);
}

fn decode_capture_snapshot(
    canonical_key: &[u8],
    envelope: &[u8],
) -> Result<GenerationPolicySnapshot, GenerationPolicyCaptureStoreErrorKind> {
    let encoded = decode_envelope(
        CAPTURE_ENVELOPE_MAGIC,
        CAPTURE_ENVELOPE_VERSION,
        canonical_key,
        envelope,
    )
    .map_err(|_| GenerationPolicyCaptureStoreErrorKind::Corrupt)?;
    let encoded = SecretMaterial::new(encoded.to_vec());
    GenerationPolicySnapshot::decode_confidential(&encoded)
        .map_err(|_| GenerationPolicyCaptureStoreErrorKind::Corrupt)
}

fn user_for(prefix: &str, canonical_key: &[u8]) -> String {
    let digest = Sha256::digest(canonical_key);
    let mut user = String::with_capacity(prefix.len() + digest.len() * 2);
    user.push_str(prefix);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        user.push(HEX[usize::from(byte >> 4)] as char);
        user.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    user
}

fn encode_envelope(
    magic: &[u8; 8],
    version: u8,
    canonical_key: &[u8],
    secret: &[u8],
) -> Option<Vec<u8>> {
    let key_len = u32::try_from(canonical_key.len()).ok()?;
    let secret_len = u32::try_from(secret.len()).ok()?;
    let mut envelope =
        Vec::with_capacity(magic.len() + 1 + 4 + canonical_key.len() + 4 + secret.len());
    envelope.extend_from_slice(magic);
    envelope.push(version);
    envelope.extend_from_slice(&key_len.to_be_bytes());
    envelope.extend_from_slice(canonical_key);
    envelope.extend_from_slice(&secret_len.to_be_bytes());
    envelope.extend_from_slice(secret);
    Some(envelope)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvelopeError {
    Corrupt,
    UnsupportedVersion,
    Collision,
}

impl EnvelopeError {
    fn credential_kind(self) -> CredentialStoreErrorKind {
        match self {
            Self::Corrupt => CredentialStoreErrorKind::Corrupt,
            Self::UnsupportedVersion => CredentialStoreErrorKind::UnsupportedVersion,
            Self::Collision => CredentialStoreErrorKind::Collision,
        }
    }
}

fn decode_envelope<'a>(
    magic: &[u8; 8],
    expected_version: u8,
    expected_key: &[u8],
    envelope: &'a [u8],
) -> Result<&'a [u8], EnvelopeError> {
    let Some(rest) = envelope.strip_prefix(magic) else {
        return Err(EnvelopeError::Corrupt);
    };
    let Some((&version, mut rest)) = rest.split_first() else {
        return Err(EnvelopeError::Corrupt);
    };
    if version != expected_version {
        return Err(EnvelopeError::UnsupportedVersion);
    }
    let key_len = take_u32(&mut rest)?;
    let stored_key = take_bytes(&mut rest, key_len)?;
    let secret_len = take_u32(&mut rest)?;
    let secret = take_bytes(&mut rest, secret_len)?;
    if !rest.is_empty() {
        return Err(EnvelopeError::Corrupt);
    }
    if stored_key != expected_key {
        return Err(EnvelopeError::Collision);
    }
    Ok(secret)
}

fn take_u32(bytes: &mut &[u8]) -> Result<usize, EnvelopeError> {
    let raw = take_bytes(bytes, 4)?;
    let raw: [u8; 4] = raw.try_into().map_err(|_| EnvelopeError::Corrupt)?;
    Ok(u32::from_be_bytes(raw) as usize)
}

fn take_bytes<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8], EnvelopeError> {
    if bytes.len() < len {
        return Err(EnvelopeError::Corrupt);
    }
    let (taken, rest) = bytes.split_at(len);
    *bytes = rest;
    Ok(taken)
}
