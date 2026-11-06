//! `ego resume --queue`: wait for a saved session to be idle, then hand the
//! prompt to the ordinary headless resume (`CLI.md` §2).
//!
//! Nothing here runs a turn or reads the conversation. The wait happens before
//! configuration, trust and the session open, so the turn that follows is
//! admitted against the session as it is after the previous writer finished,
//! by exactly the code `ego resume ID PROMPT` uses.
//!
//! Two locks, because they protect different things. The queue lease
//! serializes queued invocations among themselves; the writer lease probe
//! waits out any other owner — an interactive session, a plain resume, an ACP
//! connection — that never takes the queue lease. The queue lease is held
//! until the process exits, so the next queued invocation cannot probe an idle
//! session in the moment between this turn's open and its first append.

use std::time::{Duration, Instant};

use ego_core::run::RunTermination;
use ego_tools::clock::SystemClock;
use ego_tools::store::{Lease, LeaseError};

use crate::cli::{Args, SessionChoice, UsageError};
use crate::session;

/// How often a waiting invocation looks again. A lock probe is one syscall.
const POLL: Duration = Duration::from_millis(200);

/// This invocation's place in the session's queue, held until it is dropped.
#[derive(Debug)]
pub struct QueuedTurn {
    _lease: Lease,
}

/// Wait until `args` may run its turn on the selected session.
///
/// Pins `args.session` to the resolved id, so a newer session in the same
/// workspace cannot become "latest" while this invocation waits, and charges
/// the time spent waiting to `--timeout`.
pub async fn wait_for_turn(args: &mut Args) -> Result<QueuedTurn, RunTermination> {
    let usage = |error: UsageError| RunTermination::UsageError { message: error.0 };
    if args.profile.as_deref() == Some("ci") {
        return Err(usage(UsageError(
            "--queue cannot be combined with profile 'ci': the ci profile selects its own session store".into(),
        )));
    }
    let started = Instant::now();
    let timeout = args
        .timeout
        .as_deref()
        .map(crate::cli::duration_seconds)
        .transpose()
        .map_err(usage)?
        .map(Duration::from_secs);
    let workspace = crate::run::workspace(args.directory.as_ref()).map_err(usage)?;
    let store = session::jsonl(
        session::store_root(args.session_dir.as_ref()).map_err(usage)?,
        workspace.clone(),
    );
    let id = session::id_for(&args.session, &workspace, &store).map_err(usage)?;
    args.session = SessionChoice::Resume(Some(crate::cli::ResumeTarget::Id(id.to_string())));

    let lease = loop {
        match store.acquire_queue_lease(&id, session::lease_inputs(&SystemClock)) {
            Ok(lease) => break lease,
            Err(LeaseError::LeaseHeld { .. }) => pause(started, timeout).await?,
            Err(error) => {
                return Err(RunTermination::Failed {
                    message: format!("cannot queue a prompt on session {id}: {error}"),
                });
            }
        }
    };
    while store
        .writer_lease_held(&id)
        .map_err(|error| RunTermination::Failed {
            message: format!("cannot tell whether session {id} is in use: {error}"),
        })?
    {
        pause(started, timeout).await?;
    }
    if let Some(timeout) = timeout {
        // The run measures --timeout from its own start; give it only what
        // is left, rounded up to the grammar's whole seconds.
        let left = timeout.saturating_sub(started.elapsed());
        let seconds = left.as_secs() + u64::from(left.subsec_nanos() > 0);
        args.timeout = Some(format!("{}s", seconds.max(1)));
    }
    Ok(QueuedTurn { _lease: lease })
}

/// Sleep one poll interval, or report the deadline that has passed.
async fn pause(started: Instant, timeout: Option<Duration>) -> Result<(), RunTermination> {
    let sleep = match timeout {
        Some(timeout) => {
            let left = timeout.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Err(RunTermination::TimedOut);
            }
            left.min(POLL)
        }
        None => POLL,
    };
    tokio::time::sleep(sleep).await;
    Ok(())
}
