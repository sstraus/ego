//! Generic immutable blobs bound to one durable session.
//!
//! A reference contains content identity and exact length, but no path or
//! session id. The [`BlobStore`] instance is already bound to one session.

use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::digest::is_lower_sha256;

/// The fixed maximum returned by one paged blob read.
pub const BLOB_PAGE_BYTES: usize = 32 * 1024;

/// Lowercase hexadecimal SHA-256 naming one immutable blob.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct BlobId(String);

impl BlobId {
    pub fn parse(value: impl Into<String>) -> Result<Self, BlobError> {
        let value = value.into();
        if !is_lower_sha256(&value) {
            return Err(BlobError::InvalidReference { reference: value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BlobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for BlobId {
    type Error = BlobError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<BlobId> for String {
    fn from(value: BlobId) -> Self {
        value.0
    }
}

/// Content identity plus its exact byte length.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct BlobRef {
    pub id: BlobId,
    pub bytes: u64,
}

impl BlobRef {
    pub fn new(id: BlobId, bytes: u64) -> Self {
        Self { id, bytes }
    }
}

impl std::fmt::Display for BlobRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.id, self.bytes)
    }
}

impl std::str::FromStr for BlobRef {
    type Err = BlobError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (id, bytes) = value
            .split_once(':')
            .ok_or_else(|| BlobError::InvalidReference {
                reference: value.to_string(),
            })?;
        let id = BlobId::parse(id)?;
        let bytes = bytes
            .parse::<u64>()
            .map_err(|_| BlobError::InvalidReference {
                reference: value.to_string(),
            })?;
        let reference = Self::new(id, bytes);
        if reference.to_string() != value {
            return Err(BlobError::InvalidReference {
                reference: value.to_string(),
            });
        }
        Ok(reference)
    }
}

impl TryFrom<String> for BlobRef {
    type Error = BlobError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<BlobRef> for String {
    fn from(value: BlobRef) -> Self {
        value.to_string()
    }
}

/// One exact, bounded read from a blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlobRead {
    pub reference: BlobRef,
    pub offset: u64,
}

impl BlobRead {
    pub fn new(reference: BlobRef, offset: u64) -> Result<Self, BlobError> {
        if offset > reference.bytes {
            return Err(BlobError::InvalidOffset {
                offset,
                bytes: reference.bytes,
            });
        }
        Ok(Self { reference, offset })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlobReadWire {
    reference: BlobRef,
    offset: u64,
}

impl<'de> Deserialize<'de> for BlobRead {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = BlobReadWire::deserialize(deserializer)?;
        Self::new(wire.reference, wire.offset).map_err(serde::de::Error::custom)
    }
}

/// One verified page from a complete immutable blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlobPage {
    pub reference: BlobRef,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub next_offset: Option<u64>,
}

impl BlobPage {
    pub fn new(reference: BlobRef, offset: u64, bytes: Vec<u8>) -> Result<Self, BlobError> {
        if bytes.len() > BLOB_PAGE_BYTES {
            return Err(BlobError::PageTooLarge { bytes: bytes.len() });
        }
        let length = u64::try_from(bytes.len())
            .map_err(|_| BlobError::PageTooLarge { bytes: bytes.len() })?;
        let end = offset.checked_add(length).ok_or(BlobError::InvalidOffset {
            offset,
            bytes: reference.bytes,
        })?;
        if end > reference.bytes {
            return Err(BlobError::WrongLength {
                expected: reference.bytes,
                found: end,
            });
        }
        Ok(Self {
            next_offset: (end < reference.bytes).then_some(end),
            reference,
            offset,
            bytes,
        })
    }
}

/// Why a session blob could not be stored, verified, or collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobError {
    InvalidReference { reference: String },
    NotFound { reference: BlobId },
    Corrupt { expected: BlobId, found: BlobId },
    WrongLength { expected: u64, found: u64 },
    InvalidOffset { offset: u64, bytes: u64 },
    PageTooLarge { bytes: usize },
    Limit { limit: u64 },
    Io { message: String },
}

impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidReference { reference } => {
                write!(f, "invalid blob reference {reference:?}")
            }
            Self::NotFound { reference } => {
                write!(f, "blob {reference} is not present in this session")
            }
            Self::Corrupt { expected, found } => {
                write!(f, "blob {expected} failed verification (found {found})")
            }
            Self::WrongLength { expected, found } => write!(
                f,
                "blob has {found} bytes, but its reference declares {expected}"
            ),
            Self::InvalidOffset { offset, bytes } => write!(
                f,
                "blob offset {offset} exceeds the declared {bytes}-byte length"
            ),
            Self::PageTooLarge { bytes } => write!(
                f,
                "blob page has {bytes} bytes, above the fixed {BLOB_PAGE_BYTES}-byte limit"
            ),
            Self::Limit { limit } => write!(f, "blob exceeds the hard {limit}-byte limit"),
            Self::Io { message } => write!(f, "blob I/O failed: {message}"),
        }
    }
}

impl std::error::Error for BlobError {}

impl From<std::io::Error> for BlobError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            message: error.to_string(),
        }
    }
}

/// Incremental transaction for one immutable blob.
pub trait BlobWriter: Send {
    fn append(&mut self, chunk: &[u8]) -> Result<(), BlobError>;
    fn commit(self: Box<Self>) -> Result<BlobRef, BlobError>;
    fn abort(self: Box<Self>) -> Result<(), BlobError>;
}

/// Generic content-addressed storage for one already-bound session.
pub trait BlobStore: Send + Sync {
    fn begin(&self) -> Result<Box<dyn BlobWriter>, BlobError>;

    fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError>;

    fn read_page<'a>(&'a self, request: &'a BlobRead)
    -> BoxFuture<'a, Result<BlobPage, BlobError>>;

    /// Remove every committed blob not named by `reachable`.
    ///
    /// The caller owns semantic traversal. This generic port deliberately
    /// knows nothing about events, snapshots, images, hooks, or tool output.
    fn collect_garbage(&self, reachable: &[BlobRef]) -> Result<usize, BlobError>;
}
