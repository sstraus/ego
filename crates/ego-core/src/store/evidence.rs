//! Session-bound storage for complete tool output.
//!
//! The reference is content-addressed, but deliberately carries no session
//! identifier or path. An [`EvidenceStore`] is already bound to exactly one
//! session, so a reference from another session is merely absent rather than
//! a capability to address a global blob.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use futures::future::BoxFuture;

/// The one model-visible retrieval bound. Callers choose an original-byte
/// offset, never a page size.
pub const EVIDENCE_PAGE_BYTES: usize = 32 * 1024;

/// Lowercase hexadecimal SHA-256 naming one complete evidence object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct EvidenceId(String);

impl EvidenceId {
    pub fn parse(value: impl Into<String>) -> Result<Self, EvidenceError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(EvidenceError::InvalidReference { reference: value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EvidenceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for EvidenceId {
    type Error = EvidenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<EvidenceId> for String {
    fn from(value: EvidenceId) -> Self {
        value.0
    }
}

/// A complete evidence object's stable identity and checked byte length.
///
/// Serialized as the exact string [`Display`](std::fmt::Display) renders and
/// [`FromStr`](std::str::FromStr) accepts: `<sha256>:<bytes>`. This is also the
/// token rendered into a truncated process preview, so the model sees and
/// supplies one representation rather than translating it into an object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct EvidenceRef {
    pub id: EvidenceId,
    pub bytes: u64,
}

/// One exact, bounded read from a session evidence object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRead {
    pub reference: EvidenceRef,
    pub offset: u64,
}

impl EvidenceRead {
    pub fn new(reference: EvidenceRef, offset: u64) -> Result<Self, EvidenceError> {
        if offset > reference.bytes {
            return Err(EvidenceError::InvalidOffset {
                offset,
                bytes: reference.bytes,
            });
        }
        Ok(Self { reference, offset })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceReadWire {
    reference: EvidenceRef,
    offset: u64,
}

impl<'de> Deserialize<'de> for EvidenceRead {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = EvidenceReadWire::deserialize(deserializer)?;
        Self::new(wire.reference, wire.offset).map_err(serde::de::Error::custom)
    }
}

/// One verified page from a complete immutable evidence object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidencePage {
    pub reference: EvidenceRef,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub next_offset: Option<u64>,
}

impl EvidencePage {
    pub fn new(reference: EvidenceRef, offset: u64, bytes: Vec<u8>) -> Result<Self, EvidenceError> {
        validate_page(&reference, offset, &bytes)?;
        let end = page_end(offset, bytes.len(), reference.bytes)?;
        let total = reference.bytes;
        Ok(Self {
            reference,
            offset,
            bytes,
            next_offset: (end < total).then_some(end),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidencePageWire {
    reference: EvidenceRef,
    offset: u64,
    bytes: Vec<u8>,
    next_offset: Option<u64>,
}

impl<'de> Deserialize<'de> for EvidencePage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = EvidencePageWire::deserialize(deserializer)?;
        validate_page(&wire.reference, wire.offset, &wire.bytes)
            .map_err(serde::de::Error::custom)?;
        let end = page_end(wire.offset, wire.bytes.len(), wire.reference.bytes)
            .map_err(serde::de::Error::custom)?;
        let expected_next = (end < wire.reference.bytes).then_some(end);
        if wire.next_offset != expected_next {
            return Err(serde::de::Error::custom(
                "tool evidence page has untruthful next-offset metadata",
            ));
        }
        Ok(Self {
            reference: wire.reference,
            offset: wire.offset,
            bytes: wire.bytes,
            next_offset: wire.next_offset,
        })
    }
}

fn validate_page(reference: &EvidenceRef, offset: u64, bytes: &[u8]) -> Result<(), EvidenceError> {
    if offset > reference.bytes {
        return Err(EvidenceError::InvalidOffset {
            offset,
            bytes: reference.bytes,
        });
    }
    if bytes.len() > EVIDENCE_PAGE_BYTES {
        return Err(EvidenceError::PageTooLarge { bytes: bytes.len() });
    }
    page_end(offset, bytes.len(), reference.bytes)?;
    Ok(())
}

fn page_end(offset: u64, length: usize, total: u64) -> Result<u64, EvidenceError> {
    let length =
        u64::try_from(length).map_err(|_| EvidenceError::PageTooLarge { bytes: length })?;
    let found = offset
        .checked_add(length)
        .ok_or(EvidenceError::InvalidOffset {
            offset,
            bytes: total,
        })?;
    if found > total {
        return Err(EvidenceError::WrongLength {
            expected: total,
            found,
        });
    }
    Ok(found)
}

impl EvidenceRef {
    pub fn new(id: EvidenceId, bytes: u64) -> Self {
        Self { id, bytes }
    }
}

impl std::fmt::Display for EvidenceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.id, self.bytes)
    }
}

impl std::str::FromStr for EvidenceRef {
    type Err = EvidenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (id, bytes) = value
            .split_once(':')
            .ok_or_else(|| EvidenceError::InvalidReference {
                reference: value.to_string(),
            })?;
        let id = EvidenceId::parse(id)?;
        let bytes = bytes
            .parse::<u64>()
            .map_err(|_| EvidenceError::InvalidReference {
                reference: value.to_string(),
            })?;
        let reference = Self { id, bytes };
        if reference.to_string() != value {
            return Err(EvidenceError::InvalidReference {
                reference: value.to_string(),
            });
        }
        Ok(reference)
    }
}

impl TryFrom<String> for EvidenceRef {
    type Error = EvidenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<EvidenceRef> for String {
    fn from(value: EvidenceRef) -> Self {
        value.to_string()
    }
}

/// Why complete tool evidence could not be stored or retrieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceError {
    InvalidReference {
        reference: String,
    },
    NotFound {
        reference: EvidenceId,
    },
    Corrupt {
        expected: EvidenceId,
        found: EvidenceId,
    },
    WrongLength {
        expected: u64,
        found: u64,
    },
    InvalidOffset {
        offset: u64,
        bytes: u64,
    },
    PageTooLarge {
        bytes: usize,
    },
    Limit {
        limit: u64,
    },
    Io {
        message: String,
    },
}

impl std::fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidReference { reference } => {
                write!(f, "invalid tool evidence reference {reference:?}")
            }
            Self::NotFound { reference } => {
                write!(
                    f,
                    "tool evidence {reference} is not present in this session"
                )
            }
            Self::Corrupt { expected, found } => write!(
                f,
                "tool evidence {expected} failed verification (found {found})"
            ),
            Self::WrongLength { expected, found } => write!(
                f,
                "tool evidence has {found} bytes, but its reference declares {expected}"
            ),
            Self::InvalidOffset { offset, bytes } => write!(
                f,
                "tool evidence offset {offset} exceeds the declared {bytes}-byte length"
            ),
            Self::PageTooLarge { bytes } => write!(
                f,
                "tool evidence page has {bytes} bytes, above the fixed {EVIDENCE_PAGE_BYTES}-byte limit"
            ),
            Self::Limit { limit } => {
                write!(f, "tool evidence exceeds the hard {limit}-byte limit")
            }
            Self::Io { message } => write!(f, "tool evidence I/O failed: {message}"),
        }
    }
}

impl std::error::Error for EvidenceError {}

impl From<std::io::Error> for EvidenceError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            message: error.to_string(),
        }
    }
}

/// Incremental transaction for one complete evidence object.
pub trait EvidenceWriter: Send {
    fn append(&mut self, chunk: &[u8]) -> Result<(), EvidenceError>;
    fn commit(self: Box<Self>) -> Result<EvidenceRef, EvidenceError>;
    fn abort(self: Box<Self>) -> Result<(), EvidenceError>;
}

/// Complete evidence for one already-bound session.
pub trait EvidenceStore: Send + Sync {
    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError>;

    fn read_page<'a>(
        &'a self,
        request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>>;
}
