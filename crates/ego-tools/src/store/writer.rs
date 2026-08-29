//! Opening a session for writing: lease, repair, resume — in that order.
//!
//! See `docs/decisions/2026-08-21-opening-a-session-for-writing.md`. The five
//! durable-store pieces (`recover`, `resume`, the lease, and the two snapshot
//! APIs) were each correct and each uncalled; this is the composition of the
//! three that belong to opening. Snapshot writing is deliberately not here —
//! a snapshot is a checkpoint taken after events are applied, not something
//! an open does.
//!
//! ## Why repair is not opt-in
//!
//! Whether a crash happened is not observable from outside the store, so
//! "recover if damaged" is not a decision a caller can make; it can only
//! guess, and the common guess is to forget. `crates/ego-tools/tests/resume.rs`
//! measures what forgetting costs: appending to a torn log splices a complete
//! record onto a partial one, `valid_prefix` rejects that line and everything
//! behind it, and the next recovery discards every append made since the
//! crash — each of which returned `Ok`.
//!
//! This applies to **writing**, not to looking. `SessionStore::read`
//! deliberately surfaces every bad line as its own error, and a read that
//! repaired would destroy the evidence the port exists to expose.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ego_core::compact::{CompactionPublication, CompactionTargetStore};
use ego_core::event::domain::DomainEvent;
use ego_core::event::id::SessionId;
use ego_core::event::time::Timestamp;
use ego_core::run::{RunAppend, RunRevision, RunTransaction};
use ego_core::state::SessionState;
use ego_core::store::blob::BlobRef;
use ego_core::store::evidence::EvidenceError;
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use futures::stream::BoxStream;

use super::SessionEvidenceStore;
use super::jsonl_store::{JsonlStore, Repair};
use super::lease::{Lease, LeaseError};
use super::snapshot::SnapshotRejection;

/// The facts `Lease::acquire` needs and the store must not invent.
///
/// `pid`, `now` and `is_alive` are the caller's to supply — that is the seam
/// a test uses to present a deterministically stale lease without racing a
/// real process or the wall clock (`lease.rs`'s doc comment on `acquire`).
/// A store that read the clock and the process table itself to fill its own
/// signature would take that seam away and adopt a liveness policy that is
/// not its to hold. The store contributes only the path.
pub struct LeaseInputs<'a> {
    pub pid: u32,
    pub now: Timestamp,
    /// How long a holder may be silent before a caller that also believes it
    /// is dead may take the lease over.
    pub stale_after: Duration,
    pub is_alive: &'a dyn Fn(u32) -> bool,
}

/// Why opening for writing failed, and what it had already changed.
///
/// Three variants rather than one error type, because "nothing happened",
/// "the log may have been partly rewritten" and "the log was rewritten, here
/// is exactly how" are three different things to tell a human, and collapsing
/// them into `StoreError` would drop the `Repair` on the floor precisely when
/// it is the only record that the session on disk is no longer what it was.
#[derive(Debug)]
pub enum OpenError {
    /// The writer lease could not be taken. Nothing was touched.
    Lease(LeaseError),
    /// Recovery itself failed. It is idempotent and safe to retry, but what
    /// it managed to change before failing is not knowable from here.
    Recover(StoreError),
    /// Recovery succeeded and rebuilding the state then failed. `repair` is
    /// the only record that the log on disk was rewritten.
    Resume { source: StoreError, repair: Repair },
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Lease(e) => write!(f, "cannot open session for writing: {e}"),
            OpenError::Recover(e) => write!(f, "session recovery failed: {e}"),
            OpenError::Resume { source, repair } => write!(
                f,
                "session recovery rewrote the log ({} bytes, {} lines dropped) \
                 and rebuilding the state then failed: {source}",
                repair.truncated_bytes, repair.dropped_lines
            ),
        }
    }
}

impl std::error::Error for OpenError {}

/// A session opened for writing.
///
/// `repair` is a value rather than a log line on purpose: a caller decides
/// whether dropping N events is routine (the last append was interrupted) or
/// alarming (half the session vanished), and it cannot decide what it was
/// never handed. It is clean on the overwhelmingly common path.
pub struct OpenSession<'a> {
    /// The session as of the open, rebuilt after any repair.
    pub state: SessionState,
    /// What opening had to change on disk to get there.
    pub repair: Repair,
    /// Why the snapshot could not be used, if there was one and it could not.
    /// `None` means the snapshot was applied, or that replay was never going
    /// to consult one.
    pub snapshot: Option<SnapshotRejection>,
    /// The store to hand `TurnLoop`. Holds the lease; see [`SessionWriter`].
    pub writer: SessionWriter<'a>,
}

/// The only `SessionStore` whose `append` is covered by a writer lease.
///
/// It exists because a `Lease` kept as a private field of the opened session
/// would enforce nothing: a caller could take the state, drop that value —
/// releasing the lease — and hand the bare `JsonlStore` to `TurnLoop`, which
/// accepts any `&dyn SessionStore`. Making the lease-holder *be* the store
/// the loop borrows closes that: dropping the writer to escape the lease also
/// drops the only thing the loop could have written through.
///
/// It serves exactly one session. Both id-taking operations refuse another
/// with [`StoreError::WrongSession`] rather than delegating, since an append
/// to a second session would be an append the lease does not cover. `list`
/// takes no id and delegates: the catalogue belongs to the store, not to a
/// session.
pub struct SessionWriter<'a> {
    store: &'a JsonlStore,
    id: SessionId,
    /// Released on drop, which is the whole point. Never read.
    _lease: Arc<Lease>,
    /// References derived from the repaired log and accepted snapshot state.
    reachable_blobs: Vec<BlobRef>,
}

impl SessionWriter<'_> {
    /// The session this writer holds the lease for.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Bind complete tool evidence to this lease-held session.
    pub fn evidence_store(&self, hard_cap: u64) -> Result<SessionEvidenceStore, EvidenceError> {
        let store = SessionEvidenceStore::with_io(
            self.store.session_dir(&self.id),
            hard_cap,
            Arc::clone(&self.store.io),
            Arc::clone(&self._lease),
        )?;
        let mut reachable = self.reachable_blobs.clone();
        match self.store.read_events(&self.id) {
            Ok(events) => {
                reachable.extend(events.iter().flat_map(|event| event.kind.blob_references()))
            }
            Err(StoreError::NotFound) => {}
            Err(error) => {
                return Err(EvidenceError::Io {
                    message: error.to_string(),
                });
            }
        }
        store.collect_garbage(&reachable)?;
        Ok(store)
    }

    fn check(&self, asked: &SessionId) -> Result<(), StoreError> {
        if *asked == self.id {
            Ok(())
        } else {
            Err(StoreError::WrongSession {
                expected: self.id,
                asked: *asked,
            })
        }
    }
}

impl SessionStore for SessionWriter<'_> {
    fn append(&self, id: &SessionId, ev: &DomainEvent) -> Result<(), StoreError> {
        self.check(id)?;
        self.store.append(id, ev)
    }

    fn read(
        &self,
        id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        self.check(id)?;
        self.store.read(id)
    }

    fn compare_and_append_run(
        &self,
        session: &SessionId,
        expected_revision: Option<RunRevision>,
        transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        self.check(session)?;
        self.store
            .compare_and_append_run(session, expected_revision, transaction)
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        self.store.list()
    }
}

impl CompactionTargetStore for SessionWriter<'_> {
    fn publish_compaction_seed(&self, seed: &DomainEvent) -> CompactionPublication {
        self.store.publish_compaction_seed(&self.id, seed)
    }
}

impl JsonlStore {
    /// The per-session writer lease, alongside `events.jsonl`,
    /// `manifest.json` and `snapshot.json` (`ARCHITECTURE.md` §5).
    pub(super) fn lease_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join("lease")
    }

    /// Take the writer lease on `id`, repair whatever a crash left behind,
    /// and rebuild the session's state.
    ///
    /// Correct for a session that has never been written as well as for one
    /// being resumed, so there is no second "create" entry point to forget to
    /// keep in step with this one — and, more to the point, no way to start a
    /// session that skips the lease. `recover` is already safe on a session
    /// id with no log (`jsonl_store.rs`), but `resume` is not: it reads
    /// through `SessionStore::read`, which reports [`StoreError::NotFound`]
    /// for a session with no durable log. That is the right answer for the
    /// port — a reader asking for a session that does not exist is asking for
    /// nothing — and the wrong one here, where "no log yet" is the ordinary
    /// first moment of every session's life. It is absorbed as the empty
    /// state; every other failure is reported.
    ///
    /// **The order is load-bearing.** The lease is taken *first* so that
    /// nothing else can append between the repair and the caller's first
    /// write — an append landing in that window splices onto the log the
    /// repair just made whole, and silently un-repairs it. Recovery then runs
    /// before `resume`, because `resume` reads through `read_events`, which
    /// skips lines that do not decode: resuming an unrepaired log yields a
    /// state that is quietly missing its tail.
    pub fn open_writer(
        &self,
        id: &SessionId,
        lease: LeaseInputs<'_>,
    ) -> Result<OpenSession<'_>, OpenError> {
        let held = Lease::acquire(
            &self.lease_path(id),
            lease.pid,
            lease.now,
            lease.stale_after,
            lease.is_alive,
        )
        .map_err(OpenError::Lease)?;

        let repair = self.recover(id).map_err(OpenError::Recover)?;

        let (state, snapshot) = match self.resume(id) {
            Ok(resumed) => resumed,
            Err(StoreError::NotFound) => (SessionState::default(), None),
            Err(source) => return Err(OpenError::Resume { source, repair }),
        };

        let mut reachable_blobs = match self.read_events(id) {
            Ok(events) => events
                .iter()
                .flat_map(|event| event.kind.blob_references())
                .collect(),
            Err(StoreError::NotFound) => Vec::new(),
            Err(source) => return Err(OpenError::Resume { source, repair }),
        };
        reachable_blobs.extend(state.blob_references());

        Ok(OpenSession {
            state,
            repair,
            snapshot,
            writer: SessionWriter {
                store: self,
                id: *id,
                _lease: Arc::new(held),
                reachable_blobs,
            },
        })
    }
}
