//! Which session this run writes to, and where.
//!
//! `CLI.md` §3's four session flags, which §4.5 makes a mutually-exclusive
//! group — already enforced by [`SessionChoice`](crate::cli::SessionChoice)
//! being one field — plus the store the turn loop is handed.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use ego_core::event::domain::DomainEvent;
use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::state::SessionState;
use ego_core::store::session_store::{SessionMeta, SessionStore, StoreError};
use ego_tools::store::{JsonlStore, LeaseInputs, OpenSession, OsIo};
use futures::stream::BoxStream;

use crate::cli::{SessionChoice, UsageError};

/// How long a lease holder may be silent before another process may take it
/// over, given it also believes the holder is dead.
const STALE_AFTER: Duration = Duration::from_secs(60);

/// The namespace `--session NAME` derives an id in.
///
/// A named session is the *same* session every time, so the name has to become
/// an id somehow. Deriving it — UUIDv5 over the workspace root and the name —
/// rather than keeping an index means there is no second file to fall out of
/// step with the sessions themselves, and `--session build` in two different
/// workspaces is two different sessions, which is what "named session in this
/// workspace" means.
const NAMESPACE: uuid::Uuid = uuid::uuid!("8b8f5ec6-0e4b-4b6e-9a5f-2f6b2c8e6a10");

/// A `SessionStore` that keeps nothing, for `--ephemeral`.
///
/// A real type rather than an `Option<&dyn SessionStore>` threaded through the
/// loop: the loop's contract is that a failed append ends the turn, and an
/// `Option` would turn "there is no store" into a branch at every append. This
/// one accepts everything and remembers nothing, which is exactly what
/// `--ephemeral` promises.
///
/// `read` reports `NotFound` rather than an empty stream, because that is the
/// truth: there is no durable log to read, and answering "here is your empty
/// session" would let a resume silently succeed against nothing.
#[derive(Debug, Default)]
pub struct NullStore;

impl SessionStore for NullStore {
    fn append(&self, _id: &SessionId, _ev: &DomainEvent) -> Result<(), StoreError> {
        Ok(())
    }

    fn read(
        &self,
        _id: &SessionId,
    ) -> Result<BoxStream<'static, Result<DomainEvent, StoreError>>, StoreError> {
        Err(StoreError::NotFound)
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(Vec::new())
    }
}

/// Where sessions live: `--session-dir`, else `$EGO_HOME/sessions`.
pub fn store_root(explicit: Option<&PathBuf>) -> Result<PathBuf, UsageError> {
    if let Some(dir) = explicit {
        return Ok(dir.clone());
    }
    crate::config::ego_home()
        .map(|home| home.join("sessions"))
        .ok_or_else(|| {
            UsageError(
                "no home directory to keep sessions in: set EGO_HOME, pass \
                 --session-dir, or run with --ephemeral"
                    .to_string(),
            )
        })
}

/// The id this run writes under.
///
/// `store` is consulted for exactly one choice — [`SessionChoice::Resume`],
/// which selects a session that already exists rather than deriving or
/// inventing an id. It is an `Option` because `--ephemeral` has no store, and
/// those two choices cannot co-occur: they are the same field.
pub fn id_for(
    choice: &SessionChoice,
    workspace: &std::path::Path,
    store: Option<&dyn SessionStore>,
) -> Result<SessionId, UsageError> {
    match choice {
        SessionChoice::New | SessionChoice::Ephemeral => Ok(SessionId::new()),
        SessionChoice::Id(text) => SessionId::from_str(text)
            .map_err(|err| UsageError(format!("--session-id {err}: pass the id of a session"))),
        SessionChoice::Named(name) => {
            let key = format!("{}\u{0}{name}", workspace.display());
            let derived = uuid::Uuid::new_v5(&NAMESPACE, key.as_bytes());
            SessionId::from_str(&derived.to_string())
                .map_err(|err| UsageError(format!("a derived session id was not one: {err}")))
        }
        SessionChoice::Resume(named) => {
            let wanted =
                match named {
                    // Already checked by `into_resume_args`, which refuses a
                    // first positional that is not an id when a prompt follows it.
                    // Re-parsed rather than carried as a `SessionId`, so
                    // `SessionChoice` stays the shape the argument grammar
                    // produced and no second party has to keep them in step.
                    Some(text) => Some(SessionId::from_str(text).map_err(|err| {
                        UsageError(format!("`{text}` is not a session id: {err}"))
                    })?),
                    None => None,
                };
            let store = store.ok_or_else(|| {
                UsageError("`resume` needs the session store, and this run has none".to_string())
            })?;
            crate::catalogue::resolve(store, workspace, wanted)
        }
    }
}

/// Never claims a process is dead.
///
/// The workspace denies `unsafe_code`, so there is no `kill(pid, 0)` here, and
/// guessing from `/proc` is a platform's answer rather than an answer. It
/// costs less than it looks: a genuinely dead holder's `flock` is released by
/// the kernel when its file descriptor closes, so the lease is free anyway —
/// this predicate only governs the *stale-but-locked* case, where refusing is
/// the safe answer.
pub(crate) fn assume_alive(_pid: u32) -> bool {
    true
}

/// Open the durable session: lease, repair, resume — `JsonlStore::open_writer`
/// does all three in that order, and a `Repair` that changed anything is
/// reported rather than swallowed.
pub fn open<'a>(
    store: &'a JsonlStore,
    id: &SessionId,
    clock: &dyn Clock,
) -> Result<OpenSession<'a>, StoreError> {
    let inputs = LeaseInputs {
        pid: std::process::id(),
        now: clock.now(),
        stale_after: STALE_AFTER,
        is_alive: &assume_alive,
    };
    store.open_writer(id, inputs).map_err(|err| {
        // The three `OpenError` variants differ in what they already changed
        // on disk, which the message keeps; what the caller needs from here is
        // a store failure it can report as one.
        StoreError::Io(std::io::Error::other(err.to_string()))
    })
}

/// A store on disk at `root`, creating sessions on behalf of `workspace`.
///
/// `workspace` must be the value `run::workspace` produced — it is already
/// through `std::fs::canonicalize`, which is what makes a session reached
/// through a symlink, or through `/var` rather than `/private/var`, compare
/// equal to itself. Resolving it a second time here would be the second
/// canonicaliser `AGENTS.md`'s reuse-first rule exists to prevent.
pub fn jsonl(root: PathBuf, workspace: PathBuf) -> JsonlStore {
    JsonlStore::new(root, workspace, Box::new(OsIo))
}

/// The state a run starts from, and the store it writes through.
///
/// Two shapes rather than one trait object with a lifetime that outlives
/// both, because the ephemeral case genuinely owns nothing.
pub enum Opened<'a> {
    Durable(Box<OpenSession<'a>>),
    Ephemeral(NullStore),
}

impl Opened<'_> {
    pub fn store(&self) -> &dyn SessionStore {
        match self {
            Opened::Durable(open) => &open.writer,
            Opened::Ephemeral(null) => null,
        }
    }

    pub fn state(&self) -> SessionState {
        match self {
            Opened::Durable(open) => open.state.clone(),
            Opened::Ephemeral(_) => SessionState::default(),
        }
    }

    /// What opening had to repair, if anything. Reported rather than
    /// swallowed: a caller decides whether dropping N events is routine or
    /// alarming, and it cannot decide what it was never told.
    pub fn repair_note(&self) -> Option<String> {
        match self {
            Opened::Durable(open) if !open.repair.is_clean() => Some(format!(
                "the session log was repaired on the way in: {} bytes and {} \
                 lines were dropped from a torn tail",
                open.repair.truncated_bytes, open.repair.dropped_lines
            )),
            _ => None,
        }
    }
}

/// The wall clock a session is stamped from, named here so the one place that
/// reads real time is visible.
pub fn now(clock: &dyn Clock) -> Timestamp {
    clock.now()
}
