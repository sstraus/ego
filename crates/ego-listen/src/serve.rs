//! Accept-loop and connection-task ownership shared by every host protocol.

use std::future::Future;
use std::io;
use std::sync::Arc;

use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::limits::{ConnectionGate, GuardedStream};
use crate::{Accepted, Bound, Peer, Stream};

/// One accepted connection whose resource guard remains owned by its I/O.
pub struct GuardedAccepted {
    pub stream: GuardedStream<Stream>,
    pub peer: Peer,
}

/// Serve accepted connections until shutdown or a listener-level failure.
///
/// The listener owns every directly spawned handler. Shutdown drops the bound
/// endpoint, cancels and joins those handlers, then waits for guarded I/O that
/// a protocol upgrade may have moved into its own task.
pub async fn serve_connections<H, Fut, Shutdown>(
    bound: Bound,
    gate: ConnectionGate,
    handler: H,
    shutdown: Shutdown,
) -> io::Result<()>
where
    H: Fn(GuardedAccepted) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    Shutdown: Future<Output = ()> + Send,
{
    let handler = Arc::new(handler);
    let handler_stop = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let mut shutdown = Box::pin(shutdown);

    let mut failure = 'serving: loop {
        let mut accepting = Box::pin(bound.accept());
        let accepted = loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break 'serving None,
                _ = gate.cancelled() => break 'serving None,
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = joined {
                        break 'serving Some(handler_error(error));
                    }
                }
                accepted = &mut accepting => {
                    match accepted {
                        Ok(None) => continue 'serving,
                        Ok(Some(accepted)) => break accepted,
                        Err(error) => break 'serving Some(error),
                    }
                }
            }
        };

        let Accepted { stream, peer } = accepted;
        let mut guarding = Box::pin(gate.guard(stream));
        let guarded = loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break 'serving None,
                _ = gate.cancelled() => break 'serving None,
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = joined {
                        break 'serving Some(handler_error(error));
                    }
                }
                guarded = &mut guarding => {
                    match guarded {
                        Ok(guarded) => break guarded,
                        Err(_) => break 'serving None,
                    }
                }
            }
        };

        let handler = Arc::clone(&handler);
        let stop = handler_stop.clone();
        tasks.spawn(async move {
            let accepted = GuardedAccepted {
                stream: guarded,
                peer,
            };
            tokio::select! {
                biased;
                _ = stop.cancelled_owned() => {}
                _ = handler(accepted) => {}
            }
        });
    };

    drop(bound);
    gate.shutdown();
    handler_stop.cancel();

    while let Some(joined) = tasks.join_next().await {
        if let Err(error) = joined {
            retain_first(&mut failure, handler_error(error));
        }
    }

    if let Err(error) = gate.shutdown_and_drain().await {
        retain_first(&mut failure, io::Error::other(error));
    }
    failure.map_or(Ok(()), Err)
}

fn handler_error(error: JoinError) -> io::Error {
    io::Error::other(format!("an inbound connection handler failed: {error}"))
}

fn retain_first(first: &mut Option<io::Error>, later: io::Error) {
    if first.is_none() {
        *first = Some(later);
    } else {
        tracing::warn!(
            error = %later,
            "an additional listener failure occurred during shutdown"
        );
    }
}
