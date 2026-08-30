//! Identity and ordering: UUIDv7 newtypes, `Seq`, `Producer`.

use serde::{Deserialize, Serialize};
use uuid::{Uuid, Variant, Version};

use super::hook::HookId;

/// Stable, sortable session identity (UUIDv7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionId(Uuid);

impl SessionId {
    /// Generate a new identity from the current time.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// UUIDv7 identity of one requested child run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SubagentInvocationId(Uuid);

impl SubagentInvocationId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn from_uuid(value: Uuid) -> Result<Self, InvalidSubagentInvocationId> {
        if value.get_version() != Some(Version::SortRand) || value.get_variant() != Variant::RFC4122
        {
            return Err(InvalidSubagentInvocationId);
        }
        Ok(Self(value))
    }

    pub const fn uuid(self) -> Uuid {
        self.0
    }
}

impl Default for SubagentInvocationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::str::FromStr for SubagentInvocationId {
    type Err = InvalidSubagentInvocationId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let uuid = Uuid::parse_str(value).map_err(|_| InvalidSubagentInvocationId)?;
        let id = Self::from_uuid(uuid)?;
        if uuid.to_string() != value {
            return Err(InvalidSubagentInvocationId);
        }
        Ok(id)
    }
}

impl TryFrom<String> for SubagentInvocationId {
    type Error = InvalidSubagentInvocationId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SubagentInvocationId> for String {
    fn from(value: SubagentInvocationId) -> Self {
        value.to_string()
    }
}

impl std::fmt::Display for SubagentInvocationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidSubagentInvocationId;

impl std::fmt::Display for InvalidSubagentInvocationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid subagent invocation id; expected canonical RFC 4122 UUIDv7")
    }
}

impl std::error::Error for InvalidSubagentInvocationId {}

/// A caller's session id that is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedId(pub String);

impl std::fmt::Display for MalformedId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` is not a UUID", self.0)
    }
}

impl std::error::Error for MalformedId {}

impl std::str::FromStr for SessionId {
    type Err = MalformedId;

    /// A session id the caller supplied — `CLI.md` §3's `--session-id UUID`,
    /// "no capture-then-resume dance", and the id an ACP host names when it
    /// reattaches.
    ///
    /// Any UUID is accepted, not only a v7. The version is what makes an id
    /// this crate *mints* sortable by time, and refusing a v4 would refuse a
    /// perfectly good identity a host already has on the strength of a
    /// property nothing here depends on for correctness — the log's order is
    /// `Seq`, never the id.
    fn from_str(text: &str) -> Result<Self, MalformedId> {
        Uuid::parse_str(text)
            .map(SessionId)
            .map_err(|_| MalformedId(text.to_string()))
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable, sortable turn identity (UUIDv7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TurnId(Uuid);

impl TurnId {
    /// Generate a new identity from the current time.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable, sortable item identity (UUIDv7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ItemId(Uuid);

impl ItemId {
    /// Generate a new identity from the current time.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for ItemId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::str::FromStr for ItemId {
    type Err = MalformedId;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(text)
            .map(Self)
            .map_err(|_| MalformedId(text.to_string()))
    }
}

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable identity for one durably accepted prompt (UUIDv7).
///
/// Queue order never comes from this UUID. [`Seq`] is the authoritative
/// order; the identity correlates acceptance with its eventual
/// `UserPrompt` consumption across replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QueuedPromptId(Uuid);

impl QueuedPromptId {
    /// Mint a new queue correlation identity.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for QueuedPromptId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable, sortable event identity (UUIDv7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventId(Uuid);

impl EventId {
    /// Generate a new identity from the current time.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

/// One logical generation, stable across every retry of it (UUIDv7).
///
/// The budget settles once per `GenerationId`: a retry is the same intent
/// reaching the wire again, not a second thing to pay for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GenerationId(Uuid);

impl GenerationId {
    /// Generate a new identity from the current time.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for GenerationId {
    fn default() -> Self {
        Self::new()
    }
}

/// One wire attempt within a generation (UUIDv7).
///
/// Usage observations and corrections key to *this*, not to the generation,
/// because a provider can report a figure per attempt and summing two
/// attempts of one generation would double-count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AttemptId(Uuid);

impl AttemptId {
    /// Generate a new identity from the current time.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for AttemptId {
    fn default() -> Self {
        Self::new()
    }
}

/// The identity of a budget gate (UUIDv7).
///
/// Carried by every reservation so a token minted by one gate cannot be
/// settled against another — which would underflow the second gate's
/// outstanding balance and silently widen its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GateId(Uuid);

impl GateId {
    /// Generate a new identity from the current time.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for GateId {
    fn default() -> Self {
        Self::new()
    }
}

/// A monotonic sequence number.
///
/// `Seq` only exposes forward movement: the public API has no way to
/// construct a `Seq` that goes backwards, so "rejects regression" is a
/// structural property rather than a runtime check. `next` always
/// increments and panics on `u64` overflow (wraparound would silently
/// break the total order, which is worse than a panic in practice — u64
/// is large enough that overflow should never happen in a real process
/// lifetime).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Seq(u64);

impl Seq {
    /// The initial sequence value.
    pub const ZERO: Seq = Seq(0);

    /// Advance to the next sequence value.
    ///
    /// Panics if the counter would overflow `u64`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Seq {
        self.0 = self
            .0
            .checked_add(1)
            .expect("Seq overflowed u64, cannot advance further");
        *self
    }
}

impl Default for Seq {
    fn default() -> Self {
        Self::ZERO
    }
}

/// The identity of whatever produced an event.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Producer {
    /// The main agent loop.
    Main,
    /// A subagent, identified by the parent request that admitted it.
    Subagent(SubagentInvocationId),
    /// A hook, identified by its durable admitted identity.
    Hook(HookId),
}

impl Producer {
    /// Whether effects from this producer may themselves enter hook matching.
    /// Hook effects are deliberately non-recursive.
    pub fn may_trigger_hooks(&self) -> bool {
        !matches!(self, Self::Hook(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_sorts_by_creation() {
        let a = SessionId::new();
        let b = SessionId::new();
        let c = SessionId::new();
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn turn_id_sorts_by_creation() {
        let a = TurnId::new();
        let b = TurnId::new();
        let c = TurnId::new();
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn item_id_sorts_by_creation() {
        let a = ItemId::new();
        let b = ItemId::new();
        let c = ItemId::new();
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn event_id_sorts_by_creation() {
        let a = EventId::new();
        let b = EventId::new();
        let c = EventId::new();
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn session_id_serde_roundtrip() {
        let id = SessionId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn turn_id_serde_roundtrip() {
        let id = TurnId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: TurnId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn item_id_serde_roundtrip() {
        let id = ItemId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: ItemId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn event_id_serde_roundtrip() {
        let id = EventId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: EventId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn seq_serde_roundtrip() {
        let mut seq = Seq::default();
        seq.next();
        seq.next();
        let json = serde_json::to_string(&seq).unwrap();
        let back: Seq = serde_json::from_str(&json).unwrap();
        assert_eq!(seq, back);
    }

    #[test]
    fn producer_serde_roundtrip_main() {
        let p = Producer::Main;
        let json = serde_json::to_string(&p).unwrap();
        let back: Producer = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn producer_serde_roundtrip_subagent() {
        let invocation = SubagentInvocationId::from_uuid(
            Uuid::parse_str("01900000-0000-7000-8000-000000000042").unwrap(),
        )
        .unwrap();
        let p = Producer::Subagent(invocation);
        let json = serde_json::to_string(&p).unwrap();
        let back: Producer = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn producer_serde_roundtrip_hook() {
        let source = super::super::hook::HookSourceAuthority::Project;
        let location = super::super::hook::HookSourceLocation::parse("hooks/project.json").unwrap();
        let digest = super::super::hook::HookContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let p = Producer::Hook(HookId::derive(source, &location, &digest, 0));
        let json = serde_json::to_string(&p).unwrap();
        let back: Producer = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn seq_is_monotonic() {
        let mut seq = Seq::default();
        let first = seq.next();
        let second = seq.next();
        let third = seq.next();
        assert!(first < second);
        assert!(second < third);
    }

    #[test]
    fn seq_next_starts_above_zero() {
        let mut seq = Seq::default();
        assert_eq!(seq.next(), Seq(1));
    }

    #[test]
    #[should_panic(expected = "Seq overflowed u64, cannot advance further")]
    fn seq_panics_on_overflow() {
        let mut seq = Seq(u64::MAX);
        seq.next();
    }
}
