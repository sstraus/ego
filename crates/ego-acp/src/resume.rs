//! One pending batch per live ACP seat; all turns reuse ordinary prompt admission.
use super::*;
use futures::future::BoxFuture;
use tokio::sync::Notify;

pub(super) type PromptHandler = Arc<
    dyn Fn(
            v1::PromptRequest,
            PromptReply,
            ConnectionTo<Client>,
        ) -> BoxFuture<'static, Result<(), Error>>
        + Send
        + Sync,
>;

pub(super) enum PromptReply {
    Wire(Responder<v1::PromptResponse>),
    Resume {
        command: String,
        done: oneshot::Sender<Result<(), String>>,
    },
}
impl PromptReply {
    pub fn command(&self) -> String {
        match self {
            Self::Wire(reply) => {
                serde_json::to_string(reply.id()).expect("request id is serializable")
            }
            Self::Resume { command, .. } => command.clone(),
        }
    }
    pub fn is_resume(&self) -> bool {
        matches!(self, Self::Resume { .. })
    }
    pub fn respond(self, response: v1::PromptResponse) -> Result<(), Error> {
        match self {
            Self::Wire(reply) => reply.respond(response),
            Self::Resume { done, .. } => {
                let outcome = if response.stop_reason == v1::StopReason::EndTurn {
                    Ok(())
                } else {
                    Err(format!("result resume stopped: {:?}", response.stop_reason))
                };
                let _ = done.send(outcome);
                Ok(())
            }
        }
    }
    pub fn respond_with_error(self, error: Error) -> Result<(), Error> {
        match self {
            Self::Wire(reply) => reply.respond_with_error(error),
            Self::Resume { done, .. } => {
                let _ = done.send(Err(error.to_string()));
                Ok(())
            }
        }
    }
}

struct Pending {
    id: String,
    prompt: String,
    done: oneshot::Sender<Result<(), String>>,
}
#[derive(Clone)]
pub(super) struct ResumeQueue {
    pending: Arc<Mutex<Vec<Pending>>>,
    wake: Arc<Notify>,
}
impl ResumeQueue {
    pub fn start(
        session: v1::SessionId,
        connection: ConnectionTo<Client>,
        sessions: Sessions,
        handler: PromptHandler,
    ) -> (Self, tokio::task::AbortHandle) {
        let queue = Self {
            pending: Arc::default(),
            wake: Arc::default(),
        };
        let worker = queue.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = connection.incoming_closed() => break,
                    _ = worker.wake.notified() => {},
                }
                sessions.wait_idle(&session).await;
                if connection.is_incoming_closed() {
                    break;
                }
                let pending =
                    std::mem::take(&mut *worker.pending.lock().expect("resume queue poisoned"));
                if pending.is_empty() {
                    continue;
                }
                let command = format!(
                    "result-resume:{}",
                    pending
                        .iter()
                        .map(|p| p.id.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                );
                let prompt = pending
                    .iter()
                    .map(|p| p.prompt.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let (done, result) = oneshot::channel();
                let reply = PromptReply::Resume { command, done };
                let request = v1::PromptRequest::new(
                    session.clone(),
                    vec![v1::ContentBlock::Text(v1::TextContent::new(prompt))],
                );
                let outcome = match handler(request, reply, connection.clone()).await {
                    Ok(()) => result.await.unwrap_or_else(|_| {
                        Err("result resume lost its execution; it is not replayed".into())
                    }),
                    Err(error) => Err(error.to_string()),
                };
                for item in pending {
                    let _ = item.done.send(outcome.clone());
                }
            }
        });
        (queue, task.abort_handle())
    }
    pub fn push(&self, id: String, prompt: String) -> oneshot::Receiver<Result<(), String>> {
        let (done, result) = oneshot::channel();
        self.pending
            .lock()
            .expect("resume queue poisoned")
            .push(Pending { id, prompt, done });
        self.wake.notify_one();
        result
    }
}
