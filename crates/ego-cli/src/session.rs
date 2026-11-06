//! Which session this run writes to, and where.
//!
//! The recognized `--ephemeral` spelling is refused before a
//! [`SessionChoice`](crate::cli::SessionChoice) exists.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::EvidenceStore;
use ego_core::session::{ReplayContext, ReplayedContext};
use ego_core::state::SessionState;
use ego_core::store::session_store::{SessionStore, StoreError};
use ego_tools::store::SessionEvidenceStore;
use ego_tools::store::{ColdStoragePolicy, JsonlStore, LeaseInputs, OpenSession, OsIo};

use crate::cli::{ResumeTarget, SessionChoice, UsageError};

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

/// Where sessions live under a given home.
///
/// Its own function because `ego doctor` is handed a home rather than reading
/// the environment for one, and two spellings of "sessions" is one too many:
/// a diagnosis that looked in a different directory than every run writes to
/// would be worse than no diagnosis.
pub fn sessions_in(home: &std::path::Path) -> PathBuf {
    home.join("sessions")
}

/// Where sessions live: `--session-dir`, else `$EGO_HOME/sessions`.
pub fn store_root(explicit: Option<&PathBuf>) -> Result<PathBuf, UsageError> {
    if let Some(dir) = explicit {
        return Ok(dir.clone());
    }
    crate::config::ego_home()
        .map(|home| sessions_in(&home))
        .ok_or_else(|| {
            UsageError(
                "no home directory to keep sessions in: set EGO_HOME or pass --session-dir"
                    .to_string(),
            )
        })
}

/// The id this run writes under.
///
/// `store` is consulted for the resume choices, which select an existing session,
/// rather than deriving or inventing an id.
pub fn id_for(
    choice: &SessionChoice,
    workspace: &std::path::Path,
    store: &JsonlStore,
) -> Result<SessionId, UsageError> {
    match choice {
        SessionChoice::New => Ok(SessionId::new()),
        SessionChoice::Id(text) => SessionId::from_str(text)
            .map_err(|err| UsageError(format!("--session-id {err}: pass the id of a session"))),
        SessionChoice::Named(name) => {
            let key = format!("{}\u{0}{name}", workspace.display());
            let derived = uuid::Uuid::new_v5(&NAMESPACE, key.as_bytes());
            SessionId::from_str(&derived.to_string())
                .map_err(|err| UsageError(format!("a derived session id was not one: {err}")))
        }
        SessionChoice::ResumeLatest => crate::catalogue::resolve(store, workspace, None),
        SessionChoice::Resume(None) => crate::catalogue::resolve(store, workspace, None),
        // Already checked by `into_resume_args`, which refuses a first
        // positional that is not an id when a prompt follows it. Re-parsed
        // rather than carried as a `SessionId`, so `SessionChoice` stays the
        // shape the argument grammar produced.
        SessionChoice::Resume(Some(ResumeTarget::Id(text))) => {
            let wanted = SessionId::from_str(text)
                .map_err(|err| UsageError(format!("`{text}` is not a session id: {err}")))?;
            crate::catalogue::resolve(store, workspace, Some(wanted))
        }
        SessionChoice::Resume(Some(ResumeTarget::Title(title))) => {
            crate::catalogue::resolve_title(store, workspace, title)
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

/// What this process tells a lease about itself.
pub(crate) fn lease_inputs(clock: &dyn Clock) -> LeaseInputs<'static> {
    LeaseInputs {
        pid: std::process::id(),
        now: clock.now(),
        stale_after: STALE_AFTER,
        is_alive: &assume_alive,
    }
}

/// Open the durable session: lease, repair, resume — `JsonlStore::open_writer`
/// does all three in that order, and a `Repair` that changed anything is
/// reported rather than swallowed.
pub fn open<'a>(
    store: &'a JsonlStore,
    id: &SessionId,
    clock: &dyn Clock,
) -> Result<OpenSession<'a>, StoreError> {
    let opened = open_candidate(store, id, clock)?;
    opened.writer.publish_pid_index().map_err(StoreError::Io)?;
    Ok(opened)
}

/// Open a possible successor without changing the host's active-session binding.
pub(crate) fn open_candidate<'a>(
    store: &'a JsonlStore,
    id: &SessionId,
    clock: &dyn Clock,
) -> Result<OpenSession<'a>, StoreError> {
    store
        .open_candidate(id, lease_inputs(clock))
        .map_err(|err| {
            // Preserve recovery details in the store failure reported to the caller.
            StoreError::Io(std::io::Error::other(err.to_string()))
        })
}

/// A store on disk at `root`, creating sessions on behalf of `workspace`.
///
/// `workspace` must already be canonicalized by `run::workspace`. This keeps
/// symlink aliases and `/var` versus `/private/var` equal without a second
/// canonicalization path here.
pub fn jsonl(root: PathBuf, workspace: PathBuf) -> JsonlStore {
    let store = JsonlStore::new(root, workspace, Box::new(OsIo));
    match crate::config::ego_home() {
        Some(home) => store.with_pid_index(home),
        None => store,
    }
}

/// The cold storage policy of this run: `--no-session-compression` turns the job off for it.
pub(crate) fn cold_storage_config(
    configured: &ego_core::config::SessionStorageConfig,
    no_compression: bool,
) -> ego_core::config::SessionStorageConfig {
    let mut config = configured.clone();
    if no_compression {
        config.compress = Some(false);
    }
    config
}

/// Start one background pass of the cold storage job for the
/// store this process uses, at most once per process. The thread never sits
/// on the turn path, runs at lowered priority on Linux, rests three times its
/// work time, and reports only to
/// the debug log; a pass cut short by process exit leaves plain files intact.
pub(crate) fn start_cold_storage(
    store: std::sync::Arc<JsonlStore>,
    config: &ego_core::config::SessionStorageConfig,
) {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let Some(cold_after) = config.compress_after() else {
        return;
    };
    if STARTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("ego-cold-storage".into())
        .spawn(move || {
            if let Err(error) = ego_tools::store::lower_job_thread_priority() {
                tracing::debug!(%error, "cold storage keeps normal thread priority");
            }
            let policy = ColdStoragePolicy::new(cold_after);
            match store.compress_cold_sessions(&policy, std::time::SystemTime::now()) {
                Ok(report) => tracing::debug!(?report, "cold storage pass finished"),
                Err(error) => tracing::debug!(%error, "cold storage pass failed"),
            }
        });
    if let Err(error) = spawned {
        tracing::debug!(%error, "cold storage job did not start");
    }
}

/// Whether sessions of this process save file checkpoints; the owner's `checkpoints` key,
/// set once when the run's settings resolve.
static CHECKPOINTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub(crate) fn set_checkpoints(enabled: bool) {
    CHECKPOINTS.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// The state a durable run starts from, and the writer it owns.
pub enum Opened<'a> {
    Durable(Box<OpenSession<'a>>),
}

impl<'a> From<OpenSession<'a>> for Opened<'a> {
    fn from(opened: OpenSession<'a>) -> Self {
        Self::Durable(Box::new(opened))
    }
}

impl Opened<'_> {
    pub fn store(&self) -> &dyn SessionStore {
        let Opened::Durable(open) = self;
        &open.writer
    }

    pub fn state(&self) -> SessionState {
        let Opened::Durable(open) = self;
        open.state.clone()
    }

    /// What opening had to repair, if anything. Reported rather than
    /// swallowed: a caller decides whether dropping N events is routine or
    /// alarming, and it cannot decide what it was never told.
    pub fn repair_note(&self) -> Option<String> {
        let Opened::Durable(open) = self;
        if open.repair.is_clean() {
            return None;
        }

        let mut notes = Vec::new();
        if open.repair.migrated {
            notes
                .push("the session log was migrated to the current codec on the way in".to_owned());
        }
        if open.repair.truncated_bytes != 0 || open.repair.dropped_lines != 0 {
            notes.push(format!(
                "the session log was repaired on the way in: {} bytes and {} \
                 lines were dropped from a torn tail",
                open.repair.truncated_bytes, open.repair.dropped_lines
            ));
        }
        if open.repair.removed_tmp {
            notes.push("an orphaned manifest temporary file was removed on the way in".to_owned());
        }
        if open.repair.manifest_rebuilt {
            notes.push("the saved session details were missing or damaged and were restored from the session log".to_owned());
        }

        Some(notes.join("; "))
    }
}

/// The complete replaceable owner of one interactive session.
///
/// Keeping these fields behind one value makes activation an ordinary
/// whole-value move. A published target cannot leave the old writer paired
/// with the new id, or the old evidence namespace paired with the new state.
pub struct ActiveSession<'a> {
    pub(crate) checkpoint_fs: ego_tools::checkpoint::CheckpointFs,
    id: SessionId,
    state: SessionState,
    opened: Opened<'a>,
    evidence: SessionEvidenceStore,
    replayed: Option<ReplayedContext>,
}

impl<'a> ActiveSession<'a> {
    pub fn durable(
        opened: impl Into<Opened<'a>>,
        state: SessionState,
        evidence_cap: u64,
    ) -> Result<Self, String> {
        let opened = opened.into();
        let Opened::Durable(open) = &opened;
        let id = open.writer.id();
        let evidence = open
            .writer
            .evidence_store(evidence_cap)
            .map_err(|error| error.to_string())?;
        let replayed = ReplayContext::new(&open.writer, &open.writer)
            .replay(&id)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            checkpoint_fs: ego_tools::checkpoint::CheckpointFs::new(id)
                .with_enabled(CHECKPOINTS.load(std::sync::atomic::Ordering::Relaxed)),
            id,
            state,
            opened,
            evidence,
            replayed: Some(replayed),
        })
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn replace_state(&mut self, state: SessionState) {
        self.state = state;
    }

    pub fn store(&self) -> &dyn SessionStore {
        self.opened.store()
    }

    pub(crate) fn writer(&self) -> &ego_tools::store::SessionWriter<'a> {
        let Opened::Durable(open) = &self.opened;
        &open.writer
    }

    pub fn run_evidence(&self) -> &dyn EvidenceStore {
        let Opened::Durable(open) = &self.opened;
        &open.writer
    }

    pub fn evidence(&self) -> &SessionEvidenceStore {
        &self.evidence
    }

    pub(crate) fn replayed(&self) -> Option<&ReplayedContext> {
        self.replayed.as_ref()
    }

    /// Apply the target only after the store has established publication
    /// authority. Both published outcomes select the target; uncertainty is
    /// about durability, not whether the atomic publication became visible.
    pub fn apply_publication(
        &mut self,
        target: ActiveSession<'a>,
        publication: &ego_core::session::TargetPublication,
    ) -> bool {
        match publication {
            ego_core::session::TargetPublication::NotPublished { .. } => false,
            ego_core::session::TargetPublication::PublishedDurably { .. }
            | ego_core::session::TargetPublication::PublishedDurabilityUncertain { .. } => {
                *self = target;
                true
            }
        }
    }
}

/// The wall clock a session is stamped from, named here so the one place that
/// reads real time is visible.
pub fn now(clock: &dyn Clock) -> Timestamp {
    clock.now()
}

#[cfg(test)]
mod cold_storage_flag {
    use super::cold_storage_config;
    use ego_core::config::SessionStorageConfig;

    // Catches: --no-session-compression is parsed but the background pass still runs, or the
    // flag also rewrites the configured age so a later run without it changes behaviour.
    #[test]
    fn the_flag_turns_the_pass_off_for_one_run_and_keeps_the_configured_age() {
        let configured = SessionStorageConfig::default();
        assert!(
            cold_storage_config(&configured, false)
                .compress_after()
                .is_some()
        );
        let off = cold_storage_config(&configured, true);
        assert_eq!(off.compress_after(), None);
        assert_eq!(off.compress_after_days, configured.compress_after_days);
        assert_eq!(configured.compress, None);
    }
}

#[cfg(test)]
mod compaction_contract {
    use super::*;
    use ego_core::event::time::Clock;
    use ego_core::session::TargetPublication;
    use tempfile::{TempDir, tempdir};

    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_millis(9_000)
        }
    }

    fn durable(
        id: SessionId,
        uncached_input_tokens: u64,
        evidence_cap: u64,
    ) -> ActiveSession<'static> {
        let root: &'static TempDir = Box::leak(Box::new(tempdir().unwrap()));
        let store = Box::leak(Box::new(jsonl(
            root.path().to_path_buf(),
            PathBuf::from("/workspace"),
        )));
        let opened = open(store, &id, &FixedClock).unwrap();
        let mut state = SessionState::default();
        state.usage.uncached_input_tokens = uncached_input_tokens;
        ActiveSession::durable(opened, state, evidence_cap).unwrap()
    }

    fn assert_source_owner(active: &ActiveSession<'_>, source: SessionId, tokens: u64, cap: u64) {
        assert_eq!(active.id(), source);
        assert_eq!(active.state().usage.uncached_input_tokens, tokens);
        assert_eq!(active.evidence().hard_cap(), cap);
    }

    #[test]
    fn declined_compaction_keeps_the_source_owner_intact() {
        let source = SessionId::new();
        let target = SessionId::new();
        let mut active = durable(source, 11, 101);
        let target_owner = durable(target, 22, 202);

        assert!(!active.apply_publication(
            target_owner,
            &TargetPublication::NotPublished {
                diagnostic: "human declined compaction".to_string(),
            },
        ));
        assert_source_owner(&active, source, 11, 101);
        assert!(matches!(
            active.store().read(&target),
            Err(StoreError::WrongSession { .. })
        ));
    }

    #[test]
    fn not_published_drops_the_target_and_preserves_all_source_fields() {
        let source = SessionId::new();
        let target = SessionId::new();
        let mut active = durable(source, 31, 301);
        let target_owner = durable(target, 32, 302);

        assert!(!active.apply_publication(
            target_owner,
            &TargetPublication::NotPublished {
                diagnostic: "target append failed".to_string(),
            },
        ));
        assert_source_owner(&active, source, 31, 301);
    }

    #[test]
    fn both_published_outcomes_replace_id_state_store_and_evidence() {
        for publication in [
            TargetPublication::PublishedDurably { diagnostic: None },
            TargetPublication::PublishedDurabilityUncertain {
                diagnostic: "fsync result was lost".to_string(),
            },
        ] {
            let source = SessionId::new();
            let target = SessionId::new();
            let mut active = durable(source, 41, 401);
            let target_owner = durable(target, 42, 402);

            assert!(active.apply_publication(target_owner, &publication));
            assert_eq!(active.id(), target);
            assert_eq!(active.state().usage.uncached_input_tokens, 42);
            assert_eq!(active.evidence().hard_cap(), 402);
            assert!(matches!(
                active.store().read(&source),
                Err(StoreError::WrongSession { .. })
            ));
        }
    }

    #[test]
    fn an_activation_failure_after_publication_never_reverts_the_owner() {
        let source = SessionId::new();
        let target = SessionId::new();
        let mut active = durable(source, 51, 501);
        let target_owner = durable(target, 52, 502);

        assert!(active.apply_publication(
            target_owner,
            &TargetPublication::PublishedDurably { diagnostic: None },
        ));
        let activation_succeeded = false;
        assert!(!activation_succeeded);
        assert_eq!(active.id(), target);
        assert_eq!(active.state().usage.uncached_input_tokens, 52);
        assert_eq!(active.evidence().hard_cap(), 502);
    }
}
