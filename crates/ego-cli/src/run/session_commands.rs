//! `ego ls`, `export`, `fork` and `compact`: the commands that read or
//! reshape a stored session without taking a turn.
//!
//! Split out of `run.rs` because none of them opens a writer, resolves a
//! model, or drives the turn loop — they are the file's only inhabitants that
//! never touch an executor, and keeping them beside `execute` made both harder
//! to find.
//!
//! `compact` is the exception that proves the grouping: it *does* reach a
//! generation, but through `CompactionCommand`, not through the turn loop, so
//! it belongs with the session commands rather than with the drivers.

use std::path::Path;
use std::sync::Arc;

use ego_core::compact::{CompactionCommand, CompactionReceipt, CompactionTransitionRequest};
use ego_core::event::id::SessionId;
use ego_core::evidence::EvidenceAdmissionService;
use ego_core::export::Exporter;
use ego_core::public::project_public;
use ego_core::public::transcript::{TranscriptHeader, project_transcript};
use ego_core::run::{RequestId, RunId, RunTermination};
use ego_core::session::{ReplaySource, ReplaySourceError};
use ego_core::session_lifecycle::{
    ActivityFuture, CloseFuture, ClosedSession, HoldSignalFuture, LifecyclePublication,
    SessionActivity, SessionLifecycle, SessionRunLifecycle,
};
use ego_core::state::SessionState;
use ego_core::store::run_artifact::{ArtifactPublication, PublishedRunArtifact, RunArtifactStore};
use ego_core::store::session_store::{SessionEntry, SessionMeta};
use ego_tools::clock::SystemClock;
use ego_tools::store::{AtomicReplacePublication, JsonlStore, OsIo, StoreIo};
use serde_json::Value;

use super::{CliCompactionCommand, Ended, protocol_args, report, rooted_path, workspace};
use crate::cli::{
    CompactArgs, ExportArgs, ExportFormat, ForkArgs, Output, SessionShape, UsageError,
};
use crate::output::{Diagnostics, Stderr, Stdout, note};
use crate::session;

/// `ego ls`: the catalogue, printed (`CLI.md` §4.6).
///
/// A `RunTermination` like everything else, so one exit-code table answers for
/// the whole binary: a store that cannot be listed is `Failed` (exit 1) and a
/// store root that cannot be located is a `UsageError` (exit 2), exactly as
/// they are for a run. There is no session and no turn, so nothing here can
/// produce the endings that are about one.
pub fn ls(args: &crate::cli::LsArgs, out: &Stdout) -> RunTermination {
    match listed(args, out) {
        Ok(()) => RunTermination::Done,
        Err(termination) => termination,
    }
}

/// Read and render one session without opening a writer or constructing a run.
pub fn export(args: &ExportArgs, out: &Stdout) -> RunTermination {
    if args.follow {
        return follow_transcript(args, out);
    }
    match exported(args) {
        Ok(bytes) if args.file.is_none() => {
            out.write(&bytes);
            match out.broken() {
                Some(message) => RunTermination::Failed { message },
                None => RunTermination::Done,
            }
        }
        Ok(bytes) => {
            let root = match workspace(args.directory.as_ref()) {
                Ok(root) => root,
                Err(error) => {
                    return RunTermination::UsageError {
                        message: error.to_string(),
                    };
                }
            };
            let path = args.file.as_ref().expect("file mode was matched above");
            match publish_export_file(&root, path, bytes.as_bytes()) {
                Ok(AtomicReplacePublication::PublishedDurably) => RunTermination::Done,
                Ok(AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic }) => {
                    RunTermination::Failed {
                        message: format!(
                            "the export file is available, but saving it to disk could not be confirmed: {diagnostic}"
                        ),
                    }
                }
                Err(message) => RunTermination::Failed { message },
            }
        }
        Err(termination) => termination,
    }
}

pub(super) fn exported(args: &ExportArgs) -> Result<String, RunTermination> {
    let store = export_store(args)?;

    if args.format == ExportFormat::Transcript {
        return transcript_snapshot(&store, args.source, args.after.unwrap_or(0), true)
            .map(|(bytes, _)| bytes);
    }

    if args.sanitize {
        let canonical = canonical_sanitized_json(&store, args.source)
            .map_err(|message| RunTermination::Failed { message })?;
        return Ok(match args.format {
            ExportFormat::Json | ExportFormat::Jsonl => canonical,
            ExportFormat::Transcript => unreachable!("handled above"),
            ExportFormat::Md => {
                format!(
                    "# ego sanitized export\n\n```json\n{}\n```\n",
                    canonical.trim_end()
                )
            }
        });
    }

    let events = ReplaySource::entries(&store, &args.source)
        .map_err(local_replay_error)?
        .ok_or_else(|| RunTermination::Failed {
            message: format!("session {} is missing", args.source),
        })?
        .into_iter()
        .filter_map(|entry| match entry {
            SessionEntry::Event(event) => project_public(&event),
            SessionEntry::RunTransaction(_)
            | SessionEntry::Evidence(_)
            | SessionEntry::NoticeDelivered(_)
            | SessionEntry::ModelSelected(_) => None,
        })
        .collect::<Vec<_>>();
    match args.format {
        ExportFormat::Transcript => unreachable!("handled above"),
        ExportFormat::Json => masked_json(&events)
            .map(|rendered| format!("{rendered}\n"))
            .map_err(local_render_error),
        ExportFormat::Jsonl => {
            let mut rendered = String::new();
            for event in events {
                let line = masked_json(&event).map_err(local_render_error)?;
                rendered.push_str(&line);
                rendered.push('\n');
            }
            Ok(rendered)
        }
        ExportFormat::Md => masked_json(&events)
            .map(|rendered| format!("# ego export\n\n```json\n{rendered}\n```\n"))
            .map_err(local_render_error),
    }
}

fn export_store(args: &ExportArgs) -> Result<JsonlStore, RunTermination> {
    let usage = |error: UsageError| RunTermination::UsageError {
        message: error.to_string(),
    };
    let root = workspace(args.directory.as_ref()).map_err(usage)?;
    let store_root = rooted_path(
        &root,
        session::store_root(args.session_dir.as_ref()).map_err(usage)?,
    );
    let store = session::jsonl(store_root, root);

    Ok(store)
}

/// Read a coherent committed prefix under the adapter's short append read lock.
fn transcript_snapshot(
    store: &JsonlStore,
    session: SessionId,
    after: u64,
    header: bool,
) -> Result<(String, u64), RunTermination> {
    #[cfg(test)]
    follow_tests::REPLAYS.with(|count| count.set(count.get() + 1));
    let entries = ReplaySource::entries(store, &session)
        .map_err(local_replay_error)?
        .ok_or_else(|| RunTermination::Failed {
            message: format!("session {session} is missing"),
        })?;
    let mut rendered = String::new();
    if header {
        let lineage = ReplaySource::lineage(store, &session).map_err(local_replay_error)?;
        rendered.push_str(
            &masked_json(&TranscriptHeader::new(session, lineage)).map_err(local_render_error)?,
        );
        rendered.push('\n');
    }
    let mut cursor = after;
    for entry in entries {
        if let SessionEntry::Event(event) = entry {
            if event.env.seq.get() <= after {
                continue;
            }
            cursor = cursor.max(event.env.seq.get());
            if let Some(event) = project_transcript(session, &event) {
                rendered.push_str(&masked_json(&event).map_err(local_render_error)?);
                rendered.push('\n');
            }
        }
    }
    Ok((rendered, cursor))
}

struct TranscriptFollow {
    cursor: u64,
    header: bool,
    revision: Option<ego_tools::store::FileRevision>,
}

impl TranscriptFollow {
    fn new(after: u64) -> Self {
        Self {
            cursor: after,
            header: true,
            revision: None,
        }
    }

    fn snapshot(
        &mut self,
        store: &JsonlStore,
        session: SessionId,
    ) -> Result<String, RunTermination> {
        let revision = store
            .log_revision(&session)
            .map_err(|error| RunTermination::Failed {
                message: format!("cannot inspect followed session {session}: {error}"),
            })?;
        if revision.is_some() && revision == self.revision {
            return Ok(String::new());
        }
        let (bytes, next) = transcript_snapshot(store, session, self.cursor, self.header)?;
        // Save the pre-read signature: a concurrent append will differ on the
        // next poll, even if it raced this coherent prefix read.
        self.revision = revision;
        self.cursor = next;
        self.header = false;
        Ok(bytes)
    }
}

fn follow_transcript(args: &ExportArgs, out: &Stdout) -> RunTermination {
    let store = match export_store(args) {
        Ok(store) => store,
        Err(error) => return error,
    };
    let mut follow = TranscriptFollow::new(args.after.unwrap_or(0));
    loop {
        match follow.snapshot(&store, args.source) {
            Ok(bytes) => out.write(&bytes),
            Err(error) => return error,
        }
        if let Some(message) = out.broken() {
            return RunTermination::Failed { message };
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn masked_json(value: &impl serde::Serialize) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(value)?;
    ego_tools::secrets::from_env().mask_json(&mut value);
    serde_json::to_string(&value)
}

/// The one canonical producer-only rendering used by both `ego export` and
/// mandatory CI publication.
fn canonical_sanitized_json(store: &JsonlStore, target: SessionId) -> Result<String, String> {
    let bundle = Exporter::new(store)
        .sanitized(target)
        .map_err(|error| format!("the sanitized export was refused: {error}"))?;
    masked_json(&bundle)
        .map(|canonical| format!("{canonical}\n"))
        .map_err(|error| format!("the sanitized export could not be rendered: {error}"))
}

pub(super) fn finalize_ci_bundle(
    session_store: &JsonlStore,
    artifact_store: &dyn RunArtifactStore,
    run_id: RunId,
    termination: &mut RunTermination,
    diagnostics: &dyn Diagnostics,
) {
    let publication = canonical_sanitized_json(session_store, run_id.session_id())
        .and_then(|bytes| {
            artifact_store
                .publish(
                    run_id,
                    PublishedRunArtifact::sanitized_bundle(bytes.into_bytes()),
                )
                .map_err(|error| error.to_string())
        })
        .and_then(|publication| match publication {
            ArtifactPublication::PublishedDurably => Ok(()),
            ArtifactPublication::PublishedDurabilityUncertain { diagnostic } => Err(format!(
                "the sanitized run bundle is available, but saving it to disk could not be confirmed: {diagnostic}"
            )),
        });
    let Err(message) = publication else {
        return;
    };
    if termination.exit_code() == 0 {
        *termination = RunTermination::Failed { message };
    } else {
        diagnostics.note(&format!(
            "sanitized run bundle publication failed: {message}"
        ));
    }
}

pub(super) fn finalize_ci_error(
    session_store: &JsonlStore,
    artifact_store: Option<&JsonlStore>,
    run_id: RunId,
    mut termination: RunTermination,
    diagnostics: &dyn Diagnostics,
) -> RunTermination {
    if let Some(artifact_store) = artifact_store {
        finalize_ci_bundle(
            session_store,
            artifact_store,
            run_id,
            &mut termination,
            diagnostics,
        );
    }
    termination
}

fn local_replay_error(error: ReplaySourceError) -> RunTermination {
    RunTermination::Failed {
        message: format!("the local export could not replay its session: {error}"),
    }
}

fn local_render_error(error: serde_json::Error) -> RunTermination {
    RunTermination::Failed {
        message: format!("the local export could not be rendered: {error}"),
    }
}

fn publish_export_file(
    root: &Path,
    requested: &Path,
    bytes: &[u8],
) -> Result<AtomicReplacePublication, String> {
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let name = absolute.file_name().ok_or_else(|| {
        format!(
            "the export target {} has no normal file name",
            requested.display()
        )
    })?;
    let parent = absolute.parent().ok_or_else(|| {
        format!(
            "the export target {} has no parent directory",
            requested.display()
        )
    })?;
    let parent = std::fs::canonicalize(parent).map_err(|error| {
        format!(
            "the export target parent {} could not be resolved: {error}",
            parent.display()
        )
    })?;
    if !parent.is_dir() {
        return Err(format!(
            "the export target parent {} is not a directory",
            parent.display()
        ));
    }
    let target = parent.join(name);
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "the export target {} is a symlink",
                target.display()
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!(
                "the export target {} is not a regular file",
                target.display()
            ));
        }
        Ok(_) => {
            let existing = OsIo
                .read(&target)
                .map_err(|error| format!("the export target could not be read: {error}"))?
                .expect("metadata proved the export target exists");
            if existing == bytes {
                return Ok(AtomicReplacePublication::PublishedDurably);
            }
            return Err(format!(
                "the export target {} already contains different bytes",
                target.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "the export target {} could not be inspected: {error}",
                target.display()
            ));
        }
    }
    OsIo.replace_atomic_publication(&target, bytes)
        .map_err(|error| format!("the export target could not be published: {error}"))
}

/// `ego fork ID`: publish one clean target at the source's latest idle event.
pub async fn fork(args: &ForkArgs, out: &Stdout) -> Ended {
    match forked(args, out).await {
        Ok(ended) => ended,
        Err(termination) => Ended::refused(termination),
    }
}

/// A one-shot command runs no session in this process; a source held by
/// another process is refused by the store's lease, not by this seam.
pub(super) struct NoLiveRuns;

impl SessionRunLifecycle for NoLiveRuns {
    fn activity(&self, _session: SessionId) -> ActivityFuture<'_> {
        Box::pin(std::future::ready(Ok(SessionActivity::Inactive)))
    }

    fn cancel_and_settle(&self, session: SessionId) -> CloseFuture<'_> {
        Box::pin(std::future::ready(Ok(ClosedSession {
            id: session,
            cancelled: false,
        })))
    }

    fn hold(&self, _session: SessionId, _request: RequestId) -> HoldSignalFuture<'_> {
        Box::pin(std::future::ready(Ok(false)))
    }
}

#[derive(serde::Serialize)]
struct CompactOutput {
    v: u32,
    source_session_id: SessionId,
    source_seq: ego_core::event::id::Seq,
    target_session_id: SessionId,
    publication: ego_core::session::TargetPublication,
    successor_start_request_id: RequestId,
}

impl From<&CompactionReceipt> for CompactOutput {
    fn from(receipt: &CompactionReceipt) -> Self {
        Self {
            v: 1,
            source_session_id: receipt.source,
            source_seq: receipt.source_seq,
            target_session_id: receipt.target,
            publication: receipt.publication.clone(),
            successor_start_request_id: receipt.successor_start_request_id,
        }
    }
}

/// Run one explicit deterministic compaction without entering the run grammar.
pub async fn compact(
    args: &CompactArgs,
    out: &Stdout,
    admission: Arc<EvidenceAdmissionService>,
) -> Ended {
    let result = async {
        let root = workspace(args.directory.as_ref())?;
        let store_root = rooted_path(&root, session::store_root(args.session_dir.as_ref())?);
        let store = Arc::new(session::jsonl(store_root, root.clone()));
        let mut runtime_args = protocol_args(args.session_dir.clone(), SessionShape::default());
        runtime_args.directory = Some(root);
        runtime_args.output = args.output;
        runtime_args.budget = args.budget.clone();
        runtime_args.max_output_tokens = args.max_output_tokens;
        let command = CliCompactionCommand::new(runtime_args, store, admission);
        command
            .compact(CompactionTransitionRequest {
                request_id: args.request_id,
                source: args.source,
            })
            .await
            .map_err(|error| UsageError(error.to_string()))
    }
    .await;

    match result {
        Ok(receipt) => {
            let rendered = match args.output {
                Output::Text => Ok(format!(
                    "compacted {} to {}",
                    receipt.source, receipt.target
                )),
                Output::Json | Output::Jsonl => {
                    serde_json::to_string(&CompactOutput::from(&receipt))
                        .map_err(|error| error.to_string())
                }
            };
            match rendered {
                Ok(line) => out.line(&line),
                Err(error) => {
                    return Ended::refused(RunTermination::Failed {
                        message: format!("the compaction receipt could not be rendered: {error}"),
                    });
                }
            }
            if let Some(diagnostic) = receipt.publication.diagnostic() {
                Stderr.note(&format!(
                    "compacted session {} is available, but saving it to disk could not be confirmed: {diagnostic}",
                    receipt.target
                ));
            }
            Ended {
                termination: RunTermination::Done,
                session: receipt.target,
                state: SessionState::default(),
                result: serde_json::to_value(CompactOutput::from(&receipt))
                    .expect("the typed compaction receipt is serializable"),
                selected_session: None,
                mcp_errors: Vec::new(),
            }
        }
        Err(error) => Ended::refused(RunTermination::Failed {
            message: error.to_string(),
        }),
    }
}

async fn forked(args: &ForkArgs, out: &Stdout) -> Result<Ended, RunTermination> {
    let usage = |error: UsageError| RunTermination::UsageError {
        message: error.to_string(),
    };
    let root = workspace(args.directory.as_ref()).map_err(usage)?;
    let store_root = rooted_path(
        &root,
        session::store_root(args.session_dir.as_ref()).map_err(usage)?,
    );
    let store = Arc::new(session::jsonl(store_root, root.clone()));
    let source =
        crate::catalogue::resolve(store.as_ref(), &root, Some(args.source)).map_err(usage)?;
    let lifecycle = SessionLifecycle::new(store, Arc::new(NoLiveRuns), Arc::new(SystemClock));
    let created = match args.at {
        Some(message) => lifecycle.fork_at(source, message).await,
        None => lifecycle.fork(source).await,
    }
    .map_err(|error| RunTermination::Failed {
        message: format!("the fork failed: {error}"),
    })?;
    Ok(render_fork_result(
        created.id,
        &created.publication,
        args.output,
        out,
    ))
}

fn render_fork_result(
    target: SessionId,
    publication: &LifecyclePublication,
    output: Output,
    out: &Stdout,
) -> Ended {
    match publication {
        LifecyclePublication::PublishedWithProjectionWarning { diagnostic }
        | LifecyclePublication::PublishedDurabilityUncertain { diagnostic } => out.note(&format!(
            "forked session {target} is available, but saving it to disk could not be confirmed: {diagnostic}"
        )),
        LifecyclePublication::Published | LifecyclePublication::NotPublished { .. } => {}
    }

    let mut ended = Ended {
        termination: RunTermination::Done,
        session: target,
        state: SessionState::default(),
        result: Value::String(target.to_string()),
        selected_session: None,
        mcp_errors: Vec::new(),
    };
    let rendered = match output {
        Output::Text => {
            out.line(&target.to_string());
            Ok(())
        }
        Output::Json | Output::Jsonl => render(out, &report(&ended), "the fork result"),
    };
    if let Err(termination) = rendered {
        ended.termination = termination;
    }
    ended
}

/// [`ls`], with the endings as an error so each step can be one line.
///
/// The `Ok(())` case is `RunTermination::Done` and nothing else, so there is
/// no ending decided here that [`ls`] does not name.
fn listed(args: &crate::cli::LsArgs, out: &Stdout) -> Result<(), RunTermination> {
    let usage = |err: UsageError| RunTermination::UsageError {
        message: err.to_string(),
    };

    let root = workspace(args.directory.as_ref()).map_err(usage)?;
    let store_root = rooted_path(
        &root,
        session::store_root(args.session_dir.as_ref()).map_err(usage)?,
    );
    // The workspace is handed over for the same reason `run` hands it over —
    // a store writes it into the header of any session it creates — even
    // though listing creates none. One constructor, one meaning.
    let store = session::jsonl(store_root, root.clone());

    let metas =
        crate::catalogue::saved_sessions(&store, out).map_err(|err| RunTermination::Failed {
            message: format!("the session store could not be listed: {err}"),
        })?;

    let metas = if args.all {
        metas
    } else {
        crate::catalogue::in_workspace(metas, &root)
    };

    match args.output {
        // §4.6's single object. Rendered even when there is nothing to list:
        // a script that gets no output cannot tell an empty store from a
        // command that failed to run.
        Output::Json => render(out, &crate::catalogue::Listing::of(&metas), "the listing")?,
        // One session per line, which is what §4.2's spelling means for a
        // list: the unit of a catalogue is a session, as the unit of a run is
        // an event.
        Output::Jsonl => {
            for meta in &metas {
                render(out, &crate::catalogue::Row::of(meta), "a session")?;
            }
        }
        Output::Text => tabulate(out, &metas, &root, args.all),
    }
    Ok(())
}

/// The catalogue as a table, and the sentence an empty one owes.
///
/// The header row is printed even for an empty store, so the person who typed
/// `ego ls` sees that the command ran. The empty case says so on stderr rather
/// than on stdout, which stays the listing — which is also why this cannot
/// fail: nothing is serialised, so there is no rendering to refuse.
fn tabulate(out: &Stdout, metas: &[SessionMeta], root: &Path, all: bool) {
    for line in crate::catalogue::table(metas, all) {
        out.line(&line);
    }
    if metas.is_empty() {
        note(&if all {
            "no sessions in this store".to_string()
        } else {
            format!(
                "no session belongs to {}; list other workspaces with 'ego ls --all'",
                root.display()
            )
        });
    }
}

/// One JSON value on one line, or the ending a value that will not serialise
/// produces.
///
/// `what` names the thing rather than the error: a `Listing` and a `Row` fail
/// for the same reason and the operator needs to know which of the two the
/// command was in the middle of.
fn render<T: serde::Serialize>(out: &Stdout, value: &T, what: &str) -> Result<(), RunTermination> {
    match serde_json::to_string(value) {
        Ok(line) => {
            out.line(&line);
            Ok(())
        }
        Err(err) => Err(RunTermination::Failed {
            message: format!("{what} could not be rendered: {err}"),
        }),
    }
}

/// Confirm at the terminal, then publish the same tombstone ACP publishes.
pub async fn rm(args: &crate::cli::RmArgs, out: &Stdout) -> Ended {
    use std::io::{IsTerminal, Write};
    let result = async {
        if !args.yes {
            if !std::io::stdin().is_terminal() {
                return Err(
                    "deletion requires confirmation; pass --yes for non-interactive use".to_owned(),
                );
            }
            eprint!("Delete session {}? [y/N] ", args.source);
            std::io::stderr()
                .flush()
                .map_err(|error| error.to_string())?;
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(|error| error.to_string())?;
            if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
                return Err("deletion cancelled".to_owned());
            }
        }
        let root = workspace(args.directory.as_ref()).map_err(|error| error.to_string())?;
        let store_root = rooted_path(
            &root,
            session::store_root(args.session_dir.as_ref()).map_err(|error| error.to_string())?,
        );
        let store = Arc::new(session::jsonl(store_root, root.clone()));
        let lifecycle = SessionLifecycle::new(store, Arc::new(NoLiveRuns), Arc::new(SystemClock));
        let publication = lifecycle
            .delete_with_force(args.source, args.force)
            .await
            .map_err(|error| error.to_string())?;
        match publication {
            LifecyclePublication::PublishedWithProjectionWarning { diagnostic }
            | LifecyclePublication::PublishedDurabilityUncertain { diagnostic } => {
                Stderr.note(&diagnostic)
            }
            _ => {}
        }
        if let Err(error) = ego_tools::checkpoint::CheckpointFs::close(&root, args.source) {
            Stderr.note(&format!(
                "session deleted, but checkpoint cleanup failed: {error}"
            ));
        }
        out.line(&format!("deleted {}", args.source));
        Ok(())
    }
    .await;
    Ended::refused(match result {
        Ok(()) => RunTermination::Done,
        Err(message) => RunTermination::Failed { message },
    })
}

#[cfg(test)]
mod follow_tests {
    use super::*;
    use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
    use ego_core::event::id::{EventId, ItemId, Producer, Seq, TurnId};
    use ego_core::event::time::Timestamp;
    use ego_core::store::session_store::SessionStore;

    thread_local! {
        pub(super) static REPLAYS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn append(store: &JsonlStore, id: SessionId, seq: Seq, text: &str) {
        store
            .append(
                &id,
                &DomainEvent {
                    env: Envelope {
                        seq,
                        id: EventId::new(),
                        turn: TurnId::new(),
                        item: Some(ItemId::new()),
                        parent: None,
                        ts: Timestamp::EPOCH,
                        producer: Producer::Main,
                    },
                    kind: EventKind::UserPrompt {
                        audio: Vec::new(),
                        text: text.into(),
                        images: Vec::new(),
                        queued_prompt: None,
                    },
                },
            )
            .unwrap();
    }

    // Catches: cached follow state hides appended events, repeats its header, or ignores replacement/corruption.
    #[test]
    fn follow_transcript_revalidates_appends_replacements_and_missing_logs() {
        let home = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(home.path(), "/workspace", Box::new(OsIo));
        let id = SessionId::new();
        let mut seq = Seq::ZERO;
        append(&store, id, seq.next(), "first");
        let mut follow = TranscriptFollow::new(1);
        let first = follow.snapshot(&store, id).unwrap();
        assert_eq!(first.lines().count(), 1, "header only after sequence one");
        append(&store, id, seq.next(), "second");
        let next = follow.snapshot(&store, id).unwrap();
        assert_eq!(
            next.lines().count(),
            1,
            "one new event and no second header"
        );
        assert!(next.contains("second"));
        assert!(follow.snapshot(&store, id).unwrap().is_empty());
        let log = home.path().join(id.to_string()).join("events.jsonl");
        let replacement = log.with_extension("replacement");
        std::fs::write(&replacement, std::fs::read(&log).unwrap()).unwrap();
        std::fs::rename(replacement, &log).unwrap();
        REPLAYS.with(|count| count.set(0));
        assert!(follow.snapshot(&store, id).unwrap().is_empty());
        assert_eq!(REPLAYS.with(std::cell::Cell::get), 1);
        std::fs::write(&log, "invalid header\n").unwrap();
        assert!(follow.snapshot(&store, id).is_err());
        std::fs::remove_file(log).unwrap();
        assert!(follow.snapshot(&store, id).is_err());
    }

    // Catches: idle export --follow replays the growing log every 250 ms.
    #[test]
    fn follow_unchanged_transcript_does_not_replay_log() {
        let home = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(home.path(), "/workspace", Box::new(OsIo));
        let id = SessionId::new();
        let mut seq = Seq::ZERO;
        append(&store, id, seq.next(), "first");
        let mut follow = TranscriptFollow::new(0);
        assert!(!follow.snapshot(&store, id).unwrap().is_empty());
        REPLAYS.with(|count| count.set(0));
        for _ in 0..4 {
            assert!(follow.snapshot(&store, id).unwrap().is_empty());
        }
        assert_eq!(
            REPLAYS.with(std::cell::Cell::get),
            0,
            "unchanged follow polls must not replay file content"
        );
    }
}
