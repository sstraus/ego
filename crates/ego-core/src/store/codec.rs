//! `StoreRecord` codec v1: pure encode/decode with an xxhash64 integrity
//! checksum over `seq` and canonical payload bytes.
//!
//! See `plans/m0a-contracts.md` Step 3. No I/O lives here.
//!
//! A session's `events.jsonl` (ARCH §5) is a sequence of lines. The first
//! line is a versioned [`HeaderV1`]; every line after that is either a
//! [`StoreRecord`] or, once the codec grows a new record shape in a later
//! version, a line this build of the codec doesn't recognise. That third
//! case is [`StoreLine::Unknown`] — its raw bytes are preserved verbatim
//! rather than dropped, so a reader on an older binary can still forward an
//! event log unchanged. This module has no rewrite/migration function: any
//! future rewrite (compaction, migration — later steps) that reads a
//! [`StoreLine::Unknown`] and does not re-emit its [`RawLine::as_bytes`]
//! untouched is *silently discarding data*, not "fixing up" the line. That
//! is a decision a rewrite must make deliberately (copy verbatim, or refuse
//! to proceed) — this codec makes silent loss structurally awkward (the raw
//! bytes are the only thing `Unknown` carries) but cannot itself prevent a
//! caller from choosing to drop them.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use xxhash_rust::xxh64::xxh64;

use crate::event::id::{Seq, SessionId};
use crate::event::time::Timestamp;

/// The codec version written into every session's header line.
pub const CODEC_VERSION: u32 = 3;

/// The fixed xxhash64 seed for record checksums. Part of the codec v1
/// contract: changing it would silently invalidate every checksum already
/// written to disk, so it is not configurable.
const CHECKSUM_SEED: u64 = 0;

/// The first line of every `events.jsonl`: `{v, session, created}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderV1 {
    /// The codec version this log was written with.
    pub v: u32,
    /// The session this log belongs to.
    pub session: SessionId,
    /// When the session was created.
    pub created: Timestamp,
}

impl HeaderV1 {
    /// Build a header for the current codec version.
    pub fn new(session: SessionId, created: Timestamp) -> Self {
        Self {
            v: CODEC_VERSION,
            session,
            created,
        }
    }
}

/// A durable record: `{seq, checksum, payload}`.
///
/// `checksum` is the xxhash64 (seed [`CHECKSUM_SEED`]) of `seq` **and**
/// `payload`'s canonical JSON bytes — the exact bytes of each value as they
/// appear on the line, not a re-serialisation of a parsed form. [`encode_record`]
/// and [`decode_line`] both preserve this: encoding hashes the bytes it is
/// about to write, and decoding hashes the raw substrings [`decode_line`]
/// found on the line (via [`RawValue`], which never re-serialises), before
/// any other part of the record is interpreted. `seq` is included precisely
/// because it is the log's authoritative ordering key (ARCH §1) and later
/// tail-repair (Step 6) makes recovery decisions from it — a corrupted
/// `seq` that stayed valid JSON would otherwise validate.
///
/// The two byte strings are combined via [`checksum_of`], which
/// length-prefixes each with a fixed 8-byte big-endian length before
/// concatenating: `seq` and `payload` are both variable-length, so hashing
/// their bytes back to back with no framing would let e.g. `seq=b"1"` +
/// `payload=b"23"` collide with `seq=b"12"` + `payload=b"3"`. The length
/// prefixes make the split unambiguous.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreRecord {
    pub seq: Seq,
    pub checksum: u64,
    pub payload: Box<RawValue>,
}

impl PartialEq for StoreRecord {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
            && self.checksum == other.checksum
            && self.payload.get() == other.payload.get()
    }
}

impl Eq for StoreRecord {}

/// The verbatim bytes of a line this build of the codec doesn't recognise
/// as either a [`HeaderV1`] or a [`StoreRecord`], but which is still
/// syntactically valid JSON. See the module doc comment for why this must
/// never be silently dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLine(Vec<u8>);

impl RawLine {
    fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The line's raw bytes, exactly as decoded.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The line's raw bytes, exactly as decoded.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

/// One decoded line of `events.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreLine {
    Header(HeaderV1),
    Record(StoreRecord),
    Unknown(RawLine),
}

/// A decode failure confined to a single line, with no notion of where in
/// the log that line was — see [`decode_log`] for the layer that attaches
/// a position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The line is not valid JSON, is not a JSON object, or is missing a
    /// field required by the shape its keys indicate. Covers truncated
    /// lines (a half-written final line reads as an EOF parse error).
    Malformed(String),
    /// The record's stored checksum does not match the xxhash64 of its
    /// `seq` and payload bytes: `seq`, `payload`, or the checksum field
    /// itself, was corrupted.
    ChecksumMismatch { expected: u64, actual: u64 },
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::Malformed(message) => write!(f, "malformed store line: {message}"),
            CodecError::ChecksumMismatch { expected, actual } => write!(
                f,
                "checksum mismatch: expected {expected:#018x}, actual {actual:#018x}"
            ),
        }
    }
}

impl std::error::Error for CodecError {}

/// A [`CodecError`] enriched with the 0-based index of the line it came
/// from within the log passed to [`decode_log`] (the header line is index
/// 0). `decode_line` sees only one line at a time and structurally cannot
/// know its position, so the index is attached here, at the layer that
/// owns line positions, rather than threaded through `decode_line` as a
/// parameter it would otherwise ignore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedCodecError {
    pub index: usize,
    pub error: CodecError,
}

impl std::fmt::Display for IndexedCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.index, self.error)
    }
}

impl std::error::Error for IndexedCodecError {}

/// Combine `seq`'s and `payload`'s canonical JSON bytes into the checksum
/// input, each length-prefixed with a fixed 8-byte big-endian length so the
/// split between them is unambiguous (see [`StoreRecord`]'s doc comment).
fn checksum_of(seq_bytes: &[u8], payload_bytes: &[u8]) -> u64 {
    let mut buf = Vec::with_capacity(16 + seq_bytes.len() + payload_bytes.len());
    buf.extend_from_slice(&(seq_bytes.len() as u64).to_be_bytes());
    buf.extend_from_slice(seq_bytes);
    buf.extend_from_slice(&(payload_bytes.len() as u64).to_be_bytes());
    buf.extend_from_slice(payload_bytes);
    xxh64(&buf, CHECKSUM_SEED)
}

/// Encode a session's header line. Infallible: every `HeaderV1` value is
/// representable in the codec's wire format.
pub fn encode_header(header: &HeaderV1) -> Vec<u8> {
    serde_json::to_vec(header).expect("HeaderV1 always serialises")
}

/// Encode one record line: `seq` plus `payload`, checksummed over both
/// their canonical bytes (see [`StoreRecord`]'s doc comment for why `seq`
/// is included and how the two are framed).
pub fn encode_record<T: Serialize>(seq: Seq, payload: &T) -> Result<Vec<u8>, CodecError> {
    let seq_bytes = serde_json::to_vec(&seq).expect("Seq always serialises");
    let payload_bytes =
        serde_json::to_vec(payload).map_err(|e| CodecError::Malformed(e.to_string()))?;
    let checksum = checksum_of(&seq_bytes, &payload_bytes);
    let payload_text =
        String::from_utf8(payload_bytes).expect("serde_json output is always valid UTF-8");
    let payload =
        RawValue::from_string(payload_text).expect("just-serialised JSON is syntactically valid");
    let record = StoreRecord {
        seq,
        checksum,
        payload,
    };
    Ok(serde_json::to_vec(&record).expect("StoreRecord always serialises"))
}

/// The record shape used only to *decode*: `seq` and `payload` are borrowed
/// raw JSON text (see [`RawValue`]), so checksumming them hashes exactly
/// the bytes found on the line — never a re-serialisation of a parsed form.
/// [`StoreRecord`] (the public, owned type) is built from this once the
/// checksum has been verified.
#[derive(Deserialize)]
struct RecordWire<'a> {
    #[serde(borrow)]
    seq: &'a RawValue,
    checksum: u64,
    #[serde(borrow)]
    payload: &'a RawValue,
}

/// Decode a single line (no trailing newline) with no knowledge of its
/// position in a larger log — see [`decode_log`] for the layer that
/// enriches errors with a line index.
///
/// Dispatch commits to a shape by key presence, then requires that shape to
/// parse successfully — it does not merely try each shape in turn and fall
/// through to [`StoreLine::Unknown`] on any failure. That distinction
/// matters: a line carrying a `checksum` key whose `seq` or `checksum`
/// value happens to be malformed (missing, wrong type, or out of `u64`
/// range — as a bit flip can produce) is a corrupted *record*, not an
/// unrecognised line, and must be a typed [`CodecError`], never silently
/// reclassified as `Unknown` and passed through. Only a line that carries
/// neither a `checksum` key (record) nor a `v` key (header) — but is still
/// valid JSON — is genuinely unrecognised. A line that isn't valid UTF-8 or
/// valid JSON at all (including a truncated final line) is a typed
/// [`CodecError::Malformed`], never a panic.
pub fn decode_line(bytes: &[u8]) -> Result<StoreLine, CodecError> {
    let text = std::str::from_utf8(bytes).map_err(|e| CodecError::Malformed(e.to_string()))?;
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| CodecError::Malformed(e.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| CodecError::Malformed("store line is not a JSON object".to_string()))?;

    if obj.contains_key("checksum") {
        let wire: RecordWire =
            serde_json::from_str(text).map_err(|e| CodecError::Malformed(e.to_string()))?;
        let actual = checksum_of(wire.seq.get().as_bytes(), wire.payload.get().as_bytes());
        if actual != wire.checksum {
            return Err(CodecError::ChecksumMismatch {
                expected: wire.checksum,
                actual,
            });
        }
        // `wire.seq` already parsed once as part of `RecordWire` above, so
        // its raw text is known-valid JSON; re-parsing it into the typed
        // `Seq` cannot fail.
        let seq: Seq = serde_json::from_str(wire.seq.get())
            .expect("seq raw text already parsed as valid JSON");
        return Ok(StoreLine::Record(StoreRecord {
            seq,
            checksum: wire.checksum,
            payload: wire.payload.to_owned(),
        }));
    }

    if obj.contains_key("v") {
        let header: HeaderV1 =
            serde_json::from_str(text).map_err(|e| CodecError::Malformed(e.to_string()))?;
        return Ok(StoreLine::Header(header));
    }

    Ok(StoreLine::Unknown(RawLine::new(bytes.to_vec())))
}

/// Decode a whole log (lines separated by `\n`; a single trailing newline
/// at end-of-file is tolerated) into its [`StoreLine`]s, in order. Stops at
/// the first line that fails to decode, reporting its 0-based index via
/// [`IndexedCodecError`] — this codec does not attempt tail repair (that is
/// a later, store-level step); a corrupt or truncated tail is a decode
/// failure here, by design.
pub fn decode_log(bytes: &[u8]) -> Result<Vec<StoreLine>, IndexedCodecError> {
    let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }

    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| decode_line(line).map_err(|error| IndexedCodecError { index, error }))
        .collect()
}
