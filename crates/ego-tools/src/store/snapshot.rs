//! Snapshots: bounded replay, so resuming a long session does not mean
//! reducing its entire log.
//!
//! See `plans/m0a-contracts.md` Step 7 and `ARCHITECTURE.md` §5. A snapshot
//! is a serialised [`SessionState`] plus the `seq` it was taken at; resuming
//! means loading it and applying only the events after that `seq`.
//!
//! ## The snapshot is a cache, and is treated like one
//!
//! The log is the authority (`ARCHITECTURE.md` §1: state is a reduction over
//! an append-only log). A snapshot is derived data that merely saves work,
//! so every failure to trust one — missing, unreadable, wrong version, wrong
//! session, inconsistent with the log — degrades to a full replay rather
//! than to an error. [`resume`] never fails *because* a snapshot was bad;
//! it only fails if the log itself cannot be read.
//!
//! That is not defensive over-engineering, it is the direct consequence of
//! regression case: `JsonlStore::recover` truncates a torn tail, so a snapshot
//! taken at seq 50 can legitimately outlive a repair that left the log
//! ending at seq 47. Trusting it would resurrect events that were
//! deliberately discarded — a snapshot that is *ahead* of its log is the
//! ordinary consequence of a crash, not a corrupt file.
//!
//! ## Why the caller drives the interval
//!
//! [`SnapshotPolicy`] decides *when* to snapshot but does not write
//! anything itself, and `JsonlStore::append` does not snapshot behind the
//! caller's back. The reason is structural: a snapshot serialises
//! `SessionState`, and the store does not have one — it writes events, it
//! does not reduce them. Only whoever holds the reduced state can produce a
//! snapshot of it, so that is who owns the decision to write one.

use std::num::NonZeroUsize;

use ego_core::event::id::{Seq, SessionId};
use ego_core::state::SessionState;
use serde::{Deserialize, Serialize};

/// The snapshot format version. Bumped when [`Snapshot`]'s shape changes;
/// an unrecognised version is ignored rather than parsed optimistically,
/// because a snapshot is a cache and replaying the log is always correct.
///
/// v2: `SessionState` gained the attempt ledger and lifecycle
/// (`docs/decisions/2026-08-21-attempt-accounting-ledger.md`). A v1 snapshot
/// has no fields for them, so it is discarded and the log is replayed — a
/// cache miss, not data loss, which is precisely what this constant buys.
///
/// v3: `ItemState` gained `seq`, `kind`, `call_id` and `args`, so that a
/// tool-use history can be rebuilt from the reduced state at all
/// (`docs/decisions/2026-08-21-tool-use-history.md`). They are required
/// fields — an item with no author or no position is not a fact anything can
/// default — so a v2 snapshot cannot be read, and does not need to be.
///
/// v4: completed tool items carry a structured `ToolResult` rather than
/// flattening their evidence into `ItemState.text`. The event log is the
/// authority, so older snapshots are discarded and rebuilt rather than
/// migrated.
pub const SNAPSHOT_VERSION: u32 = 4;

/// A reduced [`SessionState`] plus the point in the log it reduces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The format version ([`SNAPSHOT_VERSION`]).
    pub v: u32,
    /// The session this reduces. Stored so a snapshot that has been copied
    /// or misfiled cannot be applied to a different session's log.
    pub session: SessionId,
    /// The `seq` of the last event included in `state`. Replay resumes
    /// strictly after this.
    pub source_seq: Seq,
    /// The reduction of every event up to and including `source_seq`.
    pub state: SessionState,
}

impl Snapshot {
    /// Capture `state` for `session` as of `source_seq`.
    pub fn new(session: SessionId, source_seq: Seq, state: SessionState) -> Self {
        Self {
            v: SNAPSHOT_VERSION,
            session,
            source_seq,
            state,
        }
    }
}

/// Why a snapshot was not used, for the caller that wants to know whether
/// a resume was bounded or a full replay.
///
/// Every variant means "the log was replayed from the start instead" — none
/// of them is an error, and [`resume`] returns state regardless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotRejection {
    /// No snapshot file exists yet.
    Absent,
    /// The file could not be read or did not parse.
    Unreadable(String),
    /// Written by a different format version.
    Version { found: u32 },
    /// Belongs to a different session — a copied or misfiled snapshot.
    WrongSession,
    /// The snapshot is ahead of the log: it includes events the log no
    /// longer has. The ordinary case is a tail repair (regression case) that
    /// truncated events a snapshot had already absorbed.
    AheadOfLog { snapshot: Seq, log: Seq },
    /// The log does not contain the event immediately after `source_seq`,
    /// so there is no suffix to bridge the gap between the snapshot and the
    /// rest of the log.
    Gap { snapshot: Seq, next_in_log: Seq },
}

/// How often to write a snapshot, in events applied.
///
/// A count rather than a duration deliberately: replay cost is bounded by
/// the *number* of events to re-reduce, not by how long ago they happened,
/// so a wall-clock interval would leave a burst-heavy session with an
/// unbounded suffix and a quiet one writing snapshots of nothing.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotPolicy {
    every: NonZeroUsize,
}

impl SnapshotPolicy {
    /// Snapshot every `every` applied events.
    ///
    /// `NonZeroUsize` because "every 0 events" has no sensible reading: it
    /// is either an infinite loop or a silent no-op, and making the caller
    /// pick one at the type level is better than guessing here.
    pub fn every(every: NonZeroUsize) -> Self {
        Self { every }
    }

    /// Whether a snapshot is due, given how many events have been applied
    /// since the last one was written.
    pub fn is_due(&self, applied_since_last: usize) -> bool {
        applied_since_last >= self.every.get()
    }

    /// The configured interval.
    pub fn interval(&self) -> usize {
        self.every.get()
    }
}

impl Default for SnapshotPolicy {
    /// Every 500 events.
    ///
    /// Chosen as a bound on replay work, not on file size: 500 events is
    /// small enough that re-reducing them on resume is imperceptible, and
    /// large enough that a streaming turn (which can emit deltas many times
    /// a second) does not turn every burst into a state serialisation.
    fn default() -> Self {
        Self {
            every: NonZeroUsize::new(500).expect("500 is not zero"),
        }
    }
}

/// Decide whether `snapshot` may be used to resume a log whose records run
/// from `first_log_seq` to `last_log_seq`, or why it may not.
///
/// Split out from any I/O so the whole trust decision is a pure function of
/// four values, and can be tested without a filesystem.
pub fn validate(
    snapshot: &Snapshot,
    session: &SessionId,
    first_log_seq: Seq,
    last_log_seq: Seq,
) -> Result<(), SnapshotRejection> {
    if snapshot.v != SNAPSHOT_VERSION {
        return Err(SnapshotRejection::Version { found: snapshot.v });
    }
    if snapshot.session != *session {
        return Err(SnapshotRejection::WrongSession);
    }
    if snapshot.source_seq > last_log_seq {
        return Err(SnapshotRejection::AheadOfLog {
            snapshot: snapshot.source_seq,
            log: last_log_seq,
        });
    }
    // The suffix must reach back to the snapshot without a hole. A log
    // whose earliest record is exactly `source_seq + 1` is *adjacent*, not
    // gapped — that is a log trimmed to precisely the snapshot's suffix,
    // and it resumes correctly. Only a log starting later than that leaves
    // events present in neither place, which is worse than replaying
    // because the resulting state would look valid while silently missing
    // whatever fell in the hole.
    let mut adjacent = snapshot.source_seq;
    let adjacent = adjacent.next();
    if first_log_seq > adjacent {
        return Err(SnapshotRejection::Gap {
            snapshot: snapshot.source_seq,
            next_in_log: first_log_seq,
        });
    }
    Ok(())
}
