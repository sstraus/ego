//! Durable skill identity and immutable catalogue evidence.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::{Uuid, Variant, Version};

const SKILL_ID_PREFIX: &str = "sk1:";
const SKILL_ID_DOMAIN: &[u8] = b"ego.skill-id";
const SKILL_ID_GRAMMAR: &[u8] = &[1];
const SHA256_BYTES: usize = 32;
const MAX_LOCATION_BYTES: usize = 4096;

/// A malformed durable skill evidence value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEvidenceError(String);

impl SkillEvidenceError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for SkillEvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SkillEvidenceError {}

/// The trust authority that admitted one skill source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSourceAuthority {
    System,
    User,
    Project,
}

/// An authority-relative, slash-normalised logical skill resource name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SkillSourceLocation(String);

impl SkillSourceLocation {
    pub fn parse(value: &str) -> Result<Self, SkillEvidenceError> {
        value.parse()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SkillSourceLocation {
    type Err = SkillEvidenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || SkillEvidenceError::new("invalid skill source location");
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
        if invalid_location_segment(first)
            || has_drive_prefix(first)
            || segments.any(invalid_location_segment)
        {
            return Err(invalid());
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for SkillSourceLocation {
    type Error = SkillEvidenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SkillSourceLocation> for String {
    fn from(value: SkillSourceLocation) -> Self {
        value.0
    }
}

impl fmt::Display for SkillSourceLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// SHA-256 of the complete admitted `SKILL.md` source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SkillContentDigest([u8; SHA256_BYTES]);

impl SkillContentDigest {
    pub fn parse(value: &str) -> Result<Self, SkillEvidenceError> {
        value.parse()
    }

    pub fn as_bytes(&self) -> &[u8; SHA256_BYTES] {
        &self.0
    }
}

impl FromStr for SkillContentDigest {
    type Err = SkillEvidenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        decode_lower_sha256(value).map(Self)
    }
}

impl TryFrom<String> for SkillContentDigest {
    type Error = SkillEvidenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SkillContentDigest> for String {
    fn from(value: SkillContentDigest) -> Self {
        encode_hex(&value.0)
    }
}

impl fmt::Display for SkillContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_hex(&self.0))
    }
}

/// Stable skill identity derived solely from admitted source evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SkillId([u8; SHA256_BYTES]);

impl SkillId {
    pub fn derive(
        source: SkillSourceAuthority,
        location: &SkillSourceLocation,
        name: &str,
        digest: &SkillContentDigest,
    ) -> Self {
        let mut hasher = Sha256::new();
        hash_frame(&mut hasher, SKILL_ID_DOMAIN);
        hash_frame(&mut hasher, SKILL_ID_GRAMMAR);
        hash_frame(&mut hasher, source_name(source));
        hash_frame(&mut hasher, location.as_str().as_bytes());
        hash_frame(&mut hasher, name.as_bytes());
        hash_frame(&mut hasher, digest.as_bytes());
        Self(hasher.finalize().into())
    }

    pub fn parse(value: &str) -> Result<Self, SkillEvidenceError> {
        value.parse()
    }
}

impl FromStr for SkillId {
    type Err = SkillEvidenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let digest = value
            .strip_prefix(SKILL_ID_PREFIX)
            .ok_or_else(|| SkillEvidenceError::new("invalid skill id"))?;
        decode_lower_sha256(digest).map(Self)
    }
}

impl TryFrom<String> for SkillId {
    type Error = SkillEvidenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SkillId> for String {
    fn from(value: SkillId) -> Self {
        format!("{SKILL_ID_PREFIX}{}", encode_hex(&value.0))
    }
}

impl fmt::Display for SkillId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{SKILL_ID_PREFIX}{}", encode_hex(&self.0))
    }
}

/// UUIDv7 identity of one admitted skill invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SkillInvocationId(Uuid);

impl SkillInvocationId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn from_uuid(value: Uuid) -> Result<Self, SkillEvidenceError> {
        validate_uuid_v7(value, "skill invocation id")?;
        Ok(Self(value))
    }

    pub const fn uuid(self) -> Uuid {
        self.0
    }
}

impl TryFrom<Uuid> for SkillInvocationId {
    type Error = SkillEvidenceError;

    fn try_from(value: Uuid) -> Result<Self, Self::Error> {
        Self::from_uuid(value)
    }
}

impl FromStr for SkillInvocationId {
    type Err = SkillEvidenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let uuid = Uuid::parse_str(value)
            .map_err(|_| SkillEvidenceError::new("invalid skill invocation id"))?;
        let id = Self::from_uuid(uuid)?;
        if uuid.to_string() != value {
            return Err(SkillEvidenceError::new(
                "skill invocation id is not canonically encoded",
            ));
        }
        Ok(id)
    }
}

impl TryFrom<String> for SkillInvocationId {
    type Error = SkillEvidenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SkillInvocationId> for String {
    fn from(value: SkillInvocationId) -> Self {
        value.to_string()
    }
}

impl fmt::Display for SkillInvocationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Default for SkillInvocationId {
    fn default() -> Self {
        Self::new()
    }
}

/// Digest of one complete canonical skill catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SkillCatalogueGeneration([u8; SHA256_BYTES]);

impl SkillCatalogueGeneration {
    /// Derive the generation from the already canonical descriptor sequence.
    pub fn derive(evidence: &SkillCatalogueEvidence) -> Self {
        let canonical = serde_json::to_vec(evidence)
            .expect("validated skill catalogue evidence always serialises");
        Self(Sha256::digest(canonical).into())
    }
}

impl FromStr for SkillCatalogueGeneration {
    type Err = SkillEvidenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        decode_lower_sha256(value).map(Self)
    }
}

impl TryFrom<String> for SkillCatalogueGeneration {
    type Error = SkillEvidenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SkillCatalogueGeneration> for String {
    fn from(value: SkillCatalogueGeneration) -> Self {
        encode_hex(&value.0)
    }
}

impl fmt::Display for SkillCatalogueGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_hex(&self.0))
    }
}

/// Bounded model- and user-visible metadata for one admitted skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillDescriptorEvidence {
    pub id: SkillId,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    pub source: SkillSourceAuthority,
    pub location: SkillSourceLocation,
    pub source_digest: SkillContentDigest,
    pub user_invocable: bool,
    pub model_invocable: bool,
}

impl SkillDescriptorEvidence {
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillDescriptorWire {
    id: SkillId,
    name: String,
    description: String,
    #[serde(default)]
    trigger: Option<String>,
    #[serde(default)]
    argument_hint: Option<String>,
    source: SkillSourceAuthority,
    location: SkillSourceLocation,
    source_digest: SkillContentDigest,
    user_invocable: bool,
    model_invocable: bool,
}

impl<'de> Deserialize<'de> for SkillDescriptorEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = SkillDescriptorWire::deserialize(deserializer)?;
        if wire.id != SkillId::derive(wire.source, &wire.location, &wire.name, &wire.source_digest)
        {
            return Err(serde::de::Error::custom(
                "skill descriptor id does not match its source evidence",
            ));
        }
        Ok(Self {
            id: wire.id,
            name: wire.name,
            description: wire.description,
            trigger: wire.trigger,
            argument_hint: wire.argument_hint,
            source: wire.source,
            location: wire.location,
            source_digest: wire.source_digest,
            user_invocable: wire.user_invocable,
            model_invocable: wire.model_invocable,
        })
    }
}

/// One immutable ordered skill catalogue snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SkillCatalogueEvidence(Vec<SkillDescriptorEvidence>);

impl SkillCatalogueEvidence {
    pub fn new(descriptors: Vec<SkillDescriptorEvidence>) -> Result<Self, SkillEvidenceError> {
        for descriptor in &descriptors {
            if descriptor.id
                != SkillId::derive(
                    descriptor.source,
                    &descriptor.location,
                    &descriptor.name,
                    &descriptor.source_digest,
                )
            {
                return Err(SkillEvidenceError::new(
                    "skill descriptor id does not match its source evidence",
                ));
            }
        }
        validate_catalogue(&descriptors)?;
        Ok(Self(descriptors))
    }

    pub fn descriptors(&self) -> &[SkillDescriptorEvidence] {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SkillCatalogueEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let descriptors = Vec::<SkillDescriptorEvidence>::deserialize(deserializer)?;
        Self::new(descriptors).map_err(serde::de::Error::custom)
    }
}

fn validate_catalogue(descriptors: &[SkillDescriptorEvidence]) -> Result<(), SkillEvidenceError> {
    let mut identities = HashSet::with_capacity(descriptors.len());
    let mut locations = HashSet::with_capacity(descriptors.len());
    let mut previous_name: Option<&str> = None;
    for descriptor in descriptors {
        if !identities.insert(descriptor.id) {
            return Err(SkillEvidenceError::new(
                "duplicate skill identity in catalogue",
            ));
        }
        if !locations.insert((descriptor.source, descriptor.location.clone())) {
            return Err(SkillEvidenceError::new(
                "duplicate skill source location in catalogue",
            ));
        }
        if previous_name.is_some_and(|previous| previous >= descriptor.name.as_str()) {
            return Err(SkillEvidenceError::new(
                "skill catalogue is not in canonical name order",
            ));
        }
        previous_name = Some(&descriptor.name);
    }
    Ok(())
}

fn source_name(source: SkillSourceAuthority) -> &'static [u8] {
    match source {
        SkillSourceAuthority::System => b"system",
        SkillSourceAuthority::User => b"user",
        SkillSourceAuthority::Project => b"project",
    }
}

fn invalid_location_segment(segment: &str) -> bool {
    segment.is_empty() || matches!(segment, "." | "..")
}

fn has_drive_prefix(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn hash_frame(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn validate_uuid_v7(value: Uuid, name: &str) -> Result<(), SkillEvidenceError> {
    if value.get_version() != Some(Version::SortRand) || value.get_variant() != Variant::RFC4122 {
        return Err(SkillEvidenceError::new(format!("invalid {name}")));
    }
    Ok(())
}

fn decode_lower_sha256(value: &str) -> Result<[u8; SHA256_BYTES], SkillEvidenceError> {
    if value.len() != SHA256_BYTES * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(SkillEvidenceError::new("invalid SHA-256 digest"));
    }
    let mut decoded = [0_u8; SHA256_BYTES];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        decoded[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    Ok(decoded)
}

fn hex_nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        _ => unreachable!("validated lowercase hexadecimal digit"),
    }
}

fn encode_hex(bytes: &[u8; SHA256_BYTES]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(SHA256_BYTES * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
