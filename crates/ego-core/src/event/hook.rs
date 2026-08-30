//! Durable hook identity, catalogue, invocation, and settlement evidence.
//!
//! This module deliberately contains no configured handler or transport
//! shape. Those belong to the later hook runtime; the event log needs only
//! enough immutable evidence to interpret an invocation after configuration
//! changes.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::{Uuid, Variant, Version};

use super::id::{ItemId, TurnId};
use crate::digest::{is_lower_sha256, sha256_hex};
use crate::store::evidence::EvidenceRef;

pub use crate::hook::HookSource;

const HOOK_ID_PREFIX: &str = "h1:";
const HOOK_ID_DOMAIN: &[u8] = b"ego.hook-id";
const HOOK_ID_GRAMMAR: &[u8] = &[1];
const SHA256_BYTES: usize = 32;
const MAX_LOCATION_BYTES: usize = 4096;
const MAX_FEEDBACK_BYTES: usize = 4096;
const MAX_FEEDBACK_SIDE_BYTES: usize = 2048;

/// A malformed durable hook evidence value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEvidenceError(String);

impl HookEvidenceError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for HookEvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HookEvidenceError {}

/// The narrow trust authority that admitted a hook descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookSourceAuthority {
    System,
    User,
    Project,
}

impl HookSourceAuthority {
    fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::System => b"system",
            Self::User => b"user",
            Self::Project => b"project",
        }
    }
}

/// An authority-relative, slash-normalised logical hook resource name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HookSourceLocation(String);

impl HookSourceLocation {
    pub fn parse(value: &str) -> Result<Self, HookEvidenceError> {
        value.parse()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the already-validated authority-relative spelling.
    pub const fn is_authority_relative(&self) -> bool {
        true
    }
}

impl FromStr for HookSourceLocation {
    type Err = HookEvidenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || HookEvidenceError::new("invalid hook source location");
        if value.is_empty()
            || value.len() > MAX_LOCATION_BYTES
            || value.starts_with('/')
            || value.ends_with('/')
            || value.contains('\\')
            || value.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        let mut segments = value.split('/');
        let Some(first) = segments.next() else {
            return Err(invalid());
        };
        if invalid_location_segment(first) || has_drive_prefix(first) {
            return Err(invalid());
        }
        if segments.any(invalid_location_segment) {
            return Err(invalid());
        }
        Ok(Self(value.to_string()))
    }
}

fn invalid_location_segment(segment: &str) -> bool {
    segment.is_empty() || matches!(segment, "." | "..")
}

fn has_drive_prefix(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

impl TryFrom<String> for HookSourceLocation {
    type Error = HookEvidenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<HookSourceLocation> for String {
    fn from(value: HookSourceLocation) -> Self {
        value.0
    }
}

impl fmt::Display for HookSourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The SHA-256 digest of the complete admitted source content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HookContentDigest([u8; SHA256_BYTES]);

impl HookContentDigest {
    pub fn parse(value: &str) -> Result<Self, HookEvidenceError> {
        value.parse()
    }

    pub fn as_bytes(&self) -> &[u8; SHA256_BYTES] {
        &self.0
    }

    /// Whether this digest has the canonical lowercase SHA-256 spelling.
    pub const fn is_lower_sha256(&self) -> bool {
        true
    }
}

impl FromStr for HookContentDigest {
    type Err = HookEvidenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        decode_lower_sha256(value).map(Self)
    }
}

impl TryFrom<String> for HookContentDigest {
    type Error = HookEvidenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<HookContentDigest> for String {
    fn from(value: HookContentDigest) -> Self {
        encode_hex(&value.0)
    }
}

impl fmt::Display for HookContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&encode_hex(&self.0))
    }
}

/// Stable hook identity derived solely from admitted source evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HookId([u8; SHA256_BYTES]);

impl HookId {
    pub fn derive(
        source: HookSourceAuthority,
        location: &HookSourceLocation,
        digest: &HookContentDigest,
        ordinal: u32,
    ) -> Self {
        let mut hasher = Sha256::new();
        hash_frame(&mut hasher, HOOK_ID_DOMAIN);
        hash_frame(&mut hasher, HOOK_ID_GRAMMAR);
        hash_frame(&mut hasher, source.as_bytes());
        hash_frame(&mut hasher, location.as_str().as_bytes());
        hash_frame(&mut hasher, digest.as_bytes());
        hash_frame(&mut hasher, &ordinal.to_be_bytes());
        Self(hasher.finalize().into())
    }

    pub fn parse(value: &str) -> Result<Self, HookEvidenceError> {
        value.parse()
    }
}

fn hash_frame(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

impl FromStr for HookId {
    type Err = HookEvidenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let digest = value
            .strip_prefix(HOOK_ID_PREFIX)
            .ok_or_else(|| HookEvidenceError::new("invalid hook id"))?;
        decode_lower_sha256(digest).map(Self)
    }
}

impl TryFrom<String> for HookId {
    type Error = HookEvidenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<HookId> for String {
    fn from(value: HookId) -> Self {
        value.to_string()
    }
}

impl fmt::Display for HookId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{HOOK_ID_PREFIX}{}", encode_hex(&self.0))
    }
}

fn decode_lower_sha256(value: &str) -> Result<[u8; SHA256_BYTES], HookEvidenceError> {
    if !is_lower_sha256(value) {
        return Err(HookEvidenceError::new("invalid lowercase SHA-256 digest"));
    }
    let mut result = [0; SHA256_BYTES];
    for (at, slot) in result.iter_mut().enumerate() {
        let offset = at * 2;
        *slot =
            (hex_nibble(value.as_bytes()[offset]) << 4) | hex_nibble(value.as_bytes()[offset + 1]);
    }
    Ok(result)
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("validated lowercase hexadecimal byte"),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

/// One external hook invocation (UUIDv7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HookInvocationId(Uuid);

impl HookInvocationId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn parse(value: &str) -> Result<Self, HookEvidenceError> {
        value.parse()
    }
}

impl Default for HookInvocationId {
    fn default() -> Self {
        Self::new()
    }
}

impl FromStr for HookInvocationId {
    type Err = HookEvidenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let uuid = Uuid::parse_str(value)
            .map_err(|_| HookEvidenceError::new("invalid hook invocation id"))?;
        if uuid.get_version() != Some(Version::SortRand) || uuid.get_variant() != Variant::RFC4122 {
            return Err(HookEvidenceError::new(
                "hook invocation id is not an RFC 4122 UUIDv7",
            ));
        }
        if uuid.to_string() != value {
            return Err(HookEvidenceError::new(
                "hook invocation id is not canonically encoded",
            ));
        }
        Ok(Self(uuid))
    }
}

impl TryFrom<String> for HookInvocationId {
    type Error = HookEvidenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<HookInvocationId> for String {
    fn from(value: HookInvocationId) -> Self {
        value.to_string()
    }
}

impl fmt::Display for HookInvocationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Minimal durable evidence for one admitted hook descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct HookDescriptorEvidence {
    id: HookId,
    source: HookSourceAuthority,
    location: HookSourceLocation,
    digest: HookContentDigest,
    ordinal: u32,
}

impl HookDescriptorEvidence {
    pub fn new(
        source: HookSourceAuthority,
        location: HookSourceLocation,
        digest: HookContentDigest,
        ordinal: u32,
    ) -> Self {
        let id = HookId::derive(source, &location, &digest, ordinal);
        Self {
            id,
            source,
            location,
            digest,
            ordinal,
        }
    }

    pub fn id(&self) -> HookId {
        self.id
    }

    pub fn source(&self) -> HookSourceAuthority {
        self.source
    }

    pub fn location(&self) -> &HookSourceLocation {
        &self.location
    }

    pub fn digest(&self) -> HookContentDigest {
        self.digest
    }

    pub fn ordinal(&self) -> u32 {
        self.ordinal
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookDescriptorWire {
    id: HookId,
    source: HookSourceAuthority,
    location: HookSourceLocation,
    digest: HookContentDigest,
    ordinal: u32,
}

impl<'de> Deserialize<'de> for HookDescriptorEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = HookDescriptorWire::deserialize(deserializer)?;
        let descriptor = Self::new(wire.source, wire.location, wire.digest, wire.ordinal);
        if descriptor.id != wire.id {
            return Err(serde::de::Error::custom(
                "hook descriptor id does not match its source evidence",
            ));
        }
        Ok(descriptor)
    }
}

/// One complete, ordered admitted hook catalogue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct HookCatalogueEvidence(Vec<HookDescriptorEvidence>);

impl HookCatalogueEvidence {
    pub fn new(descriptors: Vec<HookDescriptorEvidence>) -> Result<Self, HookEvidenceError> {
        let mut ids = HashSet::with_capacity(descriptors.len());
        let mut slots = HashSet::with_capacity(descriptors.len());
        for descriptor in &descriptors {
            if descriptor.id
                != HookId::derive(
                    descriptor.source,
                    &descriptor.location,
                    &descriptor.digest,
                    descriptor.ordinal,
                )
            {
                return Err(HookEvidenceError::new(
                    "hook descriptor id does not match its source evidence",
                ));
            }
            if !ids.insert(descriptor.id) {
                return Err(HookEvidenceError::new("duplicate hook id in catalogue"));
            }
            let slot = (
                descriptor.source,
                descriptor.location.clone(),
                descriptor.ordinal,
            );
            if !slots.insert(slot) {
                return Err(HookEvidenceError::new(
                    "duplicate hook source slot in catalogue",
                ));
            }
        }
        Ok(Self(descriptors))
    }

    pub fn as_slice(&self) -> &[HookDescriptorEvidence] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, id: HookId) -> bool {
        self.0.iter().any(|descriptor| descriptor.id == id)
    }
}

impl<'de> Deserialize<'de> for HookCatalogueEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let descriptors = Vec::<HookDescriptorEvidence>::deserialize(deserializer)?;
        Self::new(descriptors).map_err(serde::de::Error::custom)
    }
}

/// The twenty-one stable lifecycle points owned by the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookLifecycle {
    SessionStart,
    InstructionsLoaded,
    UserPromptSubmit,
    UserPromptExpansion,
    PreToolUse,
    PermissionRequest,
    PermissionDenied,
    PostToolUse,
    PostToolUseFailure,
    PostToolBatch,
    PreModelSwitch,
    PostModelSwitch,
    SubagentStart,
    SubagentStop,
    Stop,
    StopFailure,
    PreCompact,
    PostCompact,
    Elicitation,
    ElicitationResult,
    SessionEnd,
}

impl HookLifecycle {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::InstructionsLoaded => "InstructionsLoaded",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::UserPromptExpansion => "UserPromptExpansion",
            Self::PreToolUse => "PreToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::PermissionDenied => "PermissionDenied",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::PostToolBatch => "PostToolBatch",
            Self::PreModelSwitch => "PreModelSwitch",
            Self::PostModelSwitch => "PostModelSwitch",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::Stop => "Stop",
            Self::StopFailure => "StopFailure",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::Elicitation => "Elicitation",
            Self::ElicitationResult => "ElicitationResult",
            Self::SessionEnd => "SessionEnd",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "SessionStart" => Some(Self::SessionStart),
            "InstructionsLoaded" => Some(Self::InstructionsLoaded),
            "UserPromptSubmit" => Some(Self::UserPromptSubmit),
            "UserPromptExpansion" => Some(Self::UserPromptExpansion),
            "PreToolUse" => Some(Self::PreToolUse),
            "PermissionRequest" => Some(Self::PermissionRequest),
            "PermissionDenied" => Some(Self::PermissionDenied),
            "PostToolUse" => Some(Self::PostToolUse),
            "PostToolUseFailure" => Some(Self::PostToolUseFailure),
            "PostToolBatch" => Some(Self::PostToolBatch),
            "PreModelSwitch" => Some(Self::PreModelSwitch),
            "PostModelSwitch" => Some(Self::PostModelSwitch),
            "SubagentStart" => Some(Self::SubagentStart),
            "SubagentStop" => Some(Self::SubagentStop),
            "Stop" => Some(Self::Stop),
            "StopFailure" => Some(Self::StopFailure),
            "PreCompact" => Some(Self::PreCompact),
            "PostCompact" => Some(Self::PostCompact),
            "Elicitation" => Some(Self::Elicitation),
            "ElicitationResult" => Some(Self::ElicitationResult),
            "SessionEnd" => Some(Self::SessionEnd),
            _ => None,
        }
    }

    pub fn phase(self) -> HookPhase {
        match self {
            Self::UserPromptSubmit
            | Self::UserPromptExpansion
            | Self::PreToolUse
            | Self::PermissionRequest
            | Self::PostToolBatch
            | Self::PreModelSwitch
            | Self::SubagentStop
            | Self::Stop
            | Self::PreCompact
            | Self::Elicitation
            | Self::ElicitationResult => HookPhase::PreEffect,
            Self::SessionStart
            | Self::InstructionsLoaded
            | Self::PermissionDenied
            | Self::PostToolUse
            | Self::PostToolUseFailure
            | Self::PostModelSwitch
            | Self::SubagentStart
            | Self::StopFailure
            | Self::PostCompact
            | Self::SessionEnd => HookPhase::Observer,
        }
    }
}

/// Whether a lifecycle point can still prevent the subject's effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPhase {
    PreEffect,
    Observer,
}

impl HookPhase {
    pub fn is_pre_effect(self) -> bool {
        matches!(self, Self::PreEffect)
    }

    pub fn is_observer(self) -> bool {
        matches!(self, Self::Observer)
    }
}

/// The subject correlation appropriate to a hook lifecycle point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookSubject {
    Session,
    Turn { turn: TurnId },
    Item { item: ItemId },
}

/// The closed lifetime authority under which one hook invocation started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookActivation {
    Persistent,
    Skill {
        invocation: crate::event::skill::SkillInvocationId,
    },
}

/// A bounded preview whose complete bytes live in the evidence store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookFeedback {
    hook: HookId,
    #[serde(rename = "full")]
    reference: EvidenceRef,
    head: String,
    tail: Option<String>,
    omitted_bytes: u64,
}

impl HookFeedback {
    pub fn complete(
        hook: HookId,
        reference: EvidenceRef,
        text: impl Into<String>,
    ) -> Result<Self, HookEvidenceError> {
        let feedback = Self {
            hook,
            reference,
            head: text.into(),
            tail: None,
            omitted_bytes: 0,
        };
        feedback.validate()?;
        Ok(feedback)
    }

    pub fn truncated(
        hook: HookId,
        reference: EvidenceRef,
        complete: &str,
        head_bytes: usize,
        tail_bytes: usize,
        omitted_bytes: u64,
    ) -> Result<Self, HookEvidenceError> {
        validate_complete_reference(&reference, complete)?;
        let head = complete
            .get(..head_bytes)
            .ok_or_else(|| HookEvidenceError::new("feedback head splits UTF-8"))?;
        let tail_start = complete
            .len()
            .checked_sub(tail_bytes)
            .ok_or_else(|| HookEvidenceError::new("feedback tail is longer than its content"))?;
        let tail = complete
            .get(tail_start..)
            .ok_or_else(|| HookEvidenceError::new("feedback tail splits UTF-8"))?;
        let feedback = Self {
            hook,
            reference,
            head: head.to_string(),
            tail: Some(tail.to_string()),
            omitted_bytes,
        };
        feedback.validate()?;
        Ok(feedback)
    }

    pub fn hook(&self) -> HookId {
        self.hook
    }

    pub fn reference(&self) -> &EvidenceRef {
        &self.reference
    }

    pub fn head(&self) -> &str {
        &self.head
    }

    pub fn tail(&self) -> Option<&str> {
        self.tail.as_deref()
    }

    pub fn head_bytes(&self) -> usize {
        self.head.len()
    }

    pub fn tail_bytes(&self) -> usize {
        self.tail.as_ref().map_or(0, String::len)
    }

    pub fn omitted_bytes(&self) -> u64 {
        self.omitted_bytes
    }

    fn validate(&self) -> Result<(), HookEvidenceError> {
        let head_bytes = self.head.len();
        let tail_bytes = self.tail_bytes();
        if head_bytes > MAX_FEEDBACK_SIDE_BYTES
            || tail_bytes > MAX_FEEDBACK_SIDE_BYTES
            || head_bytes.saturating_add(tail_bytes) > MAX_FEEDBACK_BYTES
        {
            return Err(HookEvidenceError::new("hook feedback preview is too large"));
        }
        let retained = u64::try_from(head_bytes)
            .ok()
            .and_then(|head| {
                u64::try_from(tail_bytes)
                    .ok()
                    .and_then(|tail| head.checked_add(tail))
            })
            .ok_or_else(|| HookEvidenceError::new("hook feedback length overflow"))?;
        let accounted = retained
            .checked_add(self.omitted_bytes)
            .ok_or_else(|| HookEvidenceError::new("hook feedback length overflow"))?;
        if accounted != self.reference.bytes {
            return Err(HookEvidenceError::new(
                "hook feedback preview does not account for complete evidence length",
            ));
        }
        match &self.tail {
            None => {
                if self.omitted_bytes != 0 {
                    return Err(HookEvidenceError::new(
                        "non-truncated hook feedback claims omitted bytes",
                    ));
                }
                validate_complete_reference(&self.reference, &self.head)?;
            }
            Some(_) if self.omitted_bytes == 0 => {
                return Err(HookEvidenceError::new(
                    "truncated hook feedback omits no bytes",
                ));
            }
            Some(_) => {}
        }
        Ok(())
    }
}

fn validate_complete_reference(
    reference: &EvidenceRef,
    complete: &str,
) -> Result<(), HookEvidenceError> {
    if reference.bytes != complete.len() as u64
        || reference.id.to_string() != sha256_hex(complete.as_bytes())
    {
        return Err(HookEvidenceError::new(
            "hook feedback reference does not match complete content",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookFeedbackWire {
    hook: HookId,
    full: EvidenceRef,
    head: String,
    tail: Option<String>,
    omitted_bytes: u64,
}

impl<'de> Deserialize<'de> for HookFeedback {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = HookFeedbackWire::deserialize(deserializer)?;
        let feedback = Self {
            hook: wire.hook,
            reference: wire.full,
            head: wire.head,
            tail: wire.tail,
            omitted_bytes: wire.omitted_bytes,
        };
        feedback.validate().map_err(serde::de::Error::custom)?;
        Ok(feedback)
    }
}

/// The decision one successfully settled hook returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookDecision {
    Continue,
    Block,
    RequestEscalation,
}

impl HookDecision {
    pub fn is_continue(self) -> bool {
        matches!(self, Self::Continue)
    }

    pub fn is_blocking(self) -> bool {
        matches!(self, Self::Block)
    }

    pub fn is_escalation(self) -> bool {
        matches!(self, Self::RequestEscalation)
    }
}

/// A successful handler settlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookOutcome {
    decision: HookDecision,
    feedback: Option<HookFeedback>,
}

impl HookOutcome {
    pub fn new(decision: HookDecision, feedback: Option<HookFeedback>) -> Self {
        Self { decision, feedback }
    }

    pub fn decision(&self) -> HookDecision {
        self.decision
    }

    pub fn feedback(&self) -> Option<&HookFeedback> {
        self.feedback.as_ref()
    }
}

/// Why a configured handler failed to produce an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailureKind {
    EffectDenied,
    TimedOut,
    Cancelled,
    MalformedResult,
    BackendUnavailable,
    HandlerFailed,
    EvidenceFailed,
    DeliveryUncertain,
}

/// A failed handler settlement and any bounded diagnostic feedback it left.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookFailure {
    kind: HookFailureKind,
    feedback: Option<HookFeedback>,
}

impl HookFailure {
    pub fn new(kind: HookFailureKind, feedback: Option<HookFeedback>) -> Self {
        Self { kind, feedback }
    }

    pub fn kind(&self) -> HookFailureKind {
        self.kind
    }

    pub fn feedback(&self) -> Option<&HookFeedback> {
        self.feedback.as_ref()
    }
}

/// The one internally tagged settlement result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookResult {
    Outcome { outcome: HookOutcome },
    Failure { failure: HookFailure },
}

impl HookResult {
    pub fn feedback(&self) -> Option<&HookFeedback> {
        match self {
            Self::Outcome { outcome } => outcome.feedback(),
            Self::Failure { failure } => failure.feedback(),
        }
    }
}
