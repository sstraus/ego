//! Bounded interactive intents consumed by the sole turn coordinator.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use futures::channel::{mpsc, oneshot};
use futures::future::{Either, select};
use futures::{SinkExt, StreamExt};

pub use crate::state::QueueStatus;

// futures-channel reserves one slot per live Sender in addition to this
// buffer. Ingress clones therefore share one Sender, and a zero buffer gives
// this session exactly one queued intent globally.
const CAPACITY: usize = 0;

/// A control request could not reach or be durably accepted by its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    Closed,
    Store(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("the interactive turn coordinator is closed"),
            Self::Store(message) => write!(f, "the prompt was not durably accepted: {message}"),
        }
    }
}

impl std::error::Error for ControlError {}

pub(crate) enum ControlIntent {
    Submit {
        text: String,
        reply: oneshot::Sender<Result<QueueStatus, ControlError>>,
    },
    Interrupt {
        reply: oneshot::Sender<Result<QueueStatus, ControlError>>,
    },
}

struct Admission {
    open: AtomicBool,
}

impl Admission {
    fn open(&self) {
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
    pub async fn submit(&self, text: String) -> Result<QueueStatus, ControlError> {
        let (reply, answer) = oneshot::channel();
        let mut sender = self.sender.lock().await;
        if !self.admission.open.load(Ordering::Acquire) {
            return Err(ControlError::Closed);
        }
        sender
            .send(ControlIntent::Submit { text, reply })
            .await
            .map_err(|_| ControlError::Closed)?;
        drop(sender);
        answer.await.map_err(|_| ControlError::Closed)?
    }

    pub async fn interrupt(&self) -> Result<QueueStatus, ControlError> {
        let (reply, answer) = oneshot::channel();
        let mut sender = self.sender.lock().await;
        if !self.admission.open.load(Ordering::Acquire) {
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

    pub(crate) async fn next_or_close(&mut self) -> BoundaryIntent {
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
                Either::Right((_guard, _)) => Either::Right(()),
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
