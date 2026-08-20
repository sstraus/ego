//! Identity and ordering: UUIDv7 newtypes, `Seq`, `Producer`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    /// A subagent, identified by its id.
    Subagent(String),
    /// A hook, identified by its name.
    Hook(String),
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
        let p = Producer::Subagent("agent-42".to_string());
        let json = serde_json::to_string(&p).unwrap();
        let back: Producer = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn producer_serde_roundtrip_hook() {
        let p = Producer::Hook("pre-commit".to_string());
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
