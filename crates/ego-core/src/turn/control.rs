//! Bounded interactive intents consumed by the sole turn coordinator.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use futures::channel::{mpsc, oneshot};
use futures::future::{Either, select};
use futures::{SinkExt, StreamExt};

use crate::generator::types::ImageRef;
pub use crate::state::QueueStatus;
use crate::state::QueuedPrompt;

// futures-channel reserves one slot per live Sender in addition to this
// buffer. Ingress clones therefore share one Sender, and a zero buffer gives
// this session exactly one queued intent globally.
const CAPACITY: usize = 0;

/// A control request could not reach or be durably accepted by its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    Closed,
    Store(String),
    Unavailable(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) => f.write_str(message),
            Self::Closed => f.write_str("the interactive turn coordinator is closed"),
            Self::Store(message) => write!(f, "the prompt was not durably accepted: {message}"),
        }
    }
}

impl std::error::Error for ControlError {}

/// A durable recall and the remaining queue at that same coordinator boundary.
pub struct RecalledPrompt {
    pub prompt: QueuedPrompt,
    pub queue: QueueStatus,
    pub pending: Vec<QueuedPrompt>,
}

pub(crate) enum ControlIntent {
    RetryNow {
        reply: oneshot::Sender<Result<(), ControlError>>,
    },
    Recall {
        reply: oneshot::Sender<Result<Option<RecalledPrompt>, ControlError>>,
    },
    Background {
        reply: oneshot::Sender<Result<crate::event::id::DetachedExecId, ControlError>>,
    },
    Submit {
        text: String,
        images: Vec<ImageRef>,
        reply: oneshot::Sender<Result<QueueStatus, ControlError>>,
    },
    Interrupt {
        reply: oneshot::Sender<Result<QueueStatus, ControlError>>,
    },
}

struct Admission {
    open: AtomicBool,
    interruptible: AtomicBool,
}

impl Admission {
    fn begin_turn(&self) {
        self.interruptible.store(true, Ordering::Release);
    }

    fn open(&self) {
        self.begin_turn();
        self.open.store(true, Ordering::Release);
    }

    fn close(&self) {
        self.open.store(false, Ordering::Release);
    }
}

type SenderGate = futures::lock::Mutex<mpsc::Sender<ControlIntent>>;

/// The terminal-side async ingress. The dedicated Reedline thread retains
/// its buffer until this future returns the durable acknowledgement.
pub struct InteractiveIngress {
    sender: Arc<SenderGate>,
    admission: Arc<Admission>,
}

impl Clone for InteractiveIngress {
    fn clone(&self) -> Self {
        Self {
            sender: Arc::clone(&self.sender),
            admission: Arc::clone(&self.admission),
        }
    }
}

impl InteractiveIngress {
    /// Arm cancellation before a host exposes a newly submitted turn as busy.
    /// The host must then hand the accepted input to this session's coordinator.
    pub fn begin_turn(&self) {
        self.admission.begin_turn();
    }

    /// Request an immediate attempt only while the coordinator is in retry backoff.
    pub async fn retry_now(&self) -> Result<(), ControlError> {
        let (reply, answer) = oneshot::channel();
        let mut sender = self.sender.lock().await;
        sender
            .send(ControlIntent::RetryNow { reply })
            .await
            .map_err(|_| ControlError::Closed)?;
        drop(sender);
        answer.await.map_err(|_| ControlError::Closed)?
    }

    /// Durably withdraw the latest input still owned by the queue.
    pub async fn recall(&self) -> Result<Option<RecalledPrompt>, ControlError> {
        let (reply, answer) = oneshot::channel();
        let mut sender = self.sender.lock().await;
        if !self.admission.open.load(Ordering::Acquire) {
            return Err(ControlError::Closed);
        }
        sender
            .send(ControlIntent::Recall { reply })
            .await
            .map_err(|_| ControlError::Closed)?;
        drop(sender);
        answer.await.map_err(|_| ControlError::Closed)?
    }

    /// Accept text and already-admitted session images as one durable prompt.
    pub async fn submit(
        &self,
        text: String,
        images: Vec<ImageRef>,
    ) -> Result<QueueStatus, ControlError> {
        let (reply, answer) = oneshot::channel();
        let mut sender = self.sender.lock().await;
        if !self.admission.open.load(Ordering::Acquire) {
            return Err(ControlError::Closed);
        }
        sender
            .send(ControlIntent::Submit {
                text,
                images,
                reply,
            })
            .await
            .map_err(|_| ControlError::Closed)?;
        drop(sender);
        answer.await.map_err(|_| ControlError::Closed)?
    }

    /// Move the currently admitted exec into the session's detached registry.
    pub async fn background(&self) -> Result<crate::event::id::DetachedExecId, ControlError> {
        let (reply, answer) = oneshot::channel();
        let mut sender = self.sender.lock().await;
        if !self.admission.open.load(Ordering::Acquire) {
            return Err(ControlError::Closed);
        }
        sender
            .send(ControlIntent::Background { reply })
            .await
            .map_err(|_| ControlError::Closed)?;
        drop(sender);
        answer.await.map_err(|_| ControlError::Closed)?
    }

    pub async fn interrupt(&self) -> Result<QueueStatus, ControlError> {
        let (reply, answer) = oneshot::channel();
        let mut sender = self.sender.lock().await;
        // Checkpoint generation can close prompt admission while still owning
        // cancellation. The idle boundary closes this separate admission under
        // the same sender gate, before the host starts waiting for a new line.
        if !self.admission.interruptible.load(Ordering::Acquire) {
            return Err(ControlError::Closed);
        }
        sender
            .send(ControlIntent::Interrupt { reply })
            .await
            .map_err(|_| ControlError::Closed)?;
        drop(sender);
        answer.await.map_err(|_| ControlError::Closed)?
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ControlBoundary {
    Checkpoint,
    Idle,
}

pub(crate) enum BoundaryIntent {
    Intent(ControlIntent),
    Quiescent,
    Closed,
}

/// The core-side bounded receiver. Only `TurnLoop` consumes it.
pub struct InteractiveControl {
    receiver: mpsc::Receiver<ControlIntent>,
    status: QueueStatus,
    sender: Weak<SenderGate>,
    admission: Arc<Admission>,
}

impl InteractiveControl {
    /// Arm cancellation before a host publishes an automatic turn as busy.
    /// This does not reopen prompt admission or start a coordinator.
    pub fn begin_turn(&self) {
        self.admission.begin_turn();
    }

    pub fn status(&self) -> QueueStatus {
        self.status
    }

    pub(crate) async fn next(&mut self) -> Option<ControlIntent> {
        self.receiver.next().await
    }

    pub(crate) fn refresh(&mut self, status: QueueStatus) {
        self.status = status;
    }

    pub(crate) fn open(&mut self) {
        self.admission.open();
    }

    pub(crate) fn close(&mut self) {
        self.admission.close();
    }

    pub(crate) async fn next_or_close(&mut self, boundary: ControlBoundary) -> BoundaryIntent {
        self.admission.close();
        let Some(sender) = self.sender.upgrade() else {
            return match self.receiver.next().await {
                Some(intent) => BoundaryIntent::Intent(intent),
                None => BoundaryIntent::Closed,
            };
        };
        let raced = {
            let next = Box::pin(self.receiver.next());
            let barrier = Box::pin(sender.lock());
            match select(next, barrier).await {
                Either::Left((intent, _)) => Either::Left(intent),
                Either::Right((_guard, _)) => {
                    self.admission.interruptible.store(
                        matches!(boundary, ControlBoundary::Checkpoint),
                        Ordering::Release,
                    );
                    Either::Right(())
                }
            }
        };
        match raced {
            Either::Left(Some(intent)) => BoundaryIntent::Intent(intent),
            Either::Left(None) => BoundaryIntent::Closed,
            Either::Right(()) => BoundaryIntent::Quiescent,
        }
    }
}

/// Create the one bounded interactive ingress for a session.
pub fn interactive_control() -> (InteractiveIngress, InteractiveControl) {
    let (sender, receiver) = mpsc::channel(CAPACITY);
    let sender = Arc::new(SenderGate::new(sender));
    let admission = Arc::new(Admission {
        open: AtomicBool::new(true),
        interruptible: AtomicBool::new(false),
    });
    (
        InteractiveIngress {
            sender: Arc::clone(&sender),
            admission: Arc::clone(&admission),
        },
        InteractiveControl {
            receiver,
            status: QueueStatus::default(),
            sender: Arc::downgrade(&sender),
            admission,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{FutureExt, poll};

    // Catches: a late Escape waits forever on a retained but idle receiver,
    // or leaks an old interrupt into the next active turn.
    #[test]
    fn late_interrupt_after_quiescence_rejects_without_poisoning_the_next_turn() {
        futures::executor::block_on(async {
            let (ingress, mut control) = interactive_control();
            control.open();
            assert!(matches!(
                control.next_or_close(ControlBoundary::Idle).await,
                BoundaryIntent::Quiescent
            ));
            for _ in 0..3 {
                assert_eq!(
                    ingress.interrupt().now_or_never(),
                    Some(Err(ControlError::Closed))
                );
            }

            control.open();
            let mut prompt = Box::pin(ingress.submit("after repeated Escape".into(), Vec::new()));
            assert!(poll!(prompt.as_mut()).is_pending());
            let Some(ControlIntent::Submit { text, reply, .. }) = control.next().await else {
                panic!("repeated Escape prevented the next explicit prompt");
            };
            assert_eq!(text, "after repeated Escape");
            let _ = reply.send(Ok(control.status()));
            assert_eq!(prompt.await, Ok(control.status()));
            let mut interrupt = Box::pin(ingress.interrupt());
            assert!(poll!(interrupt.as_mut()).is_pending());
            let Some(ControlIntent::Interrupt { reply }) = control.next().await else {
                panic!("a new turn must receive only its own interrupt");
            };
            let _ = reply.send(Ok(control.status()));
            assert_eq!(interrupt.await, Ok(control.status()));
        });
    }

    // Catches: an automatic turn publishes Busy before core attachment and
    // discards immediate Escape because only explicit submissions were armed.
    #[test]
    fn automatic_turn_arming_accepts_escape_before_the_first_coordinator_poll() {
        futures::executor::block_on(async {
            let (ingress, mut control) = interactive_control();
            assert!(matches!(
                control.next_or_close(ControlBoundary::Idle).await,
                BoundaryIntent::Quiescent
            ));
            control.begin_turn();
            assert_eq!(
                ingress.submit("not admitted".into(), Vec::new()).await,
                Err(ControlError::Closed)
            );
            let mut interrupt = Box::pin(ingress.interrupt());
            assert!(poll!(interrupt.as_mut()).is_pending());
            let Some(ControlIntent::Interrupt { reply }) = control.next().await else {
                panic!("the automatic turn lost its early interrupt");
            };
            let _ = reply.send(Ok(control.status()));
            assert_eq!(interrupt.await, Ok(control.status()));
        });
    }

    // Catches: fixing late Escape by reusing prompt admission also prevents
    // cancellation during checkpoint generation, where prompt admission closes.
    #[test]
    fn prompt_closure_keeps_interrupts_until_the_settlement_barrier() {
        futures::executor::block_on(async {
            let (ingress, mut control) = interactive_control();
            control.open();
            control.close();
            assert!(matches!(
                control.next_or_close(ControlBoundary::Checkpoint).await,
                BoundaryIntent::Quiescent
            ));
            assert_eq!(
                ingress.submit("late prompt".into(), Vec::new()).await,
                Err(ControlError::Closed)
            );
            let mut interrupt = Box::pin(ingress.interrupt());
            assert!(poll!(interrupt.as_mut()).is_pending());
            let BoundaryIntent::Intent(ControlIntent::Interrupt { reply }) =
                control.next_or_close(ControlBoundary::Idle).await
            else {
                panic!("settlement must acknowledge the interrupt ordered before it");
            };
            let _ = reply.send(Ok(control.status()));
            assert_eq!(interrupt.await, Ok(control.status()));
            assert!(matches!(
                control.next_or_close(ControlBoundary::Idle).await,
                BoundaryIntent::Quiescent
            ));
            assert_eq!(
                ingress.interrupt().now_or_never(),
                Some(Err(ControlError::Closed))
            );
        });
    }
}
