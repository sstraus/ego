//! Common process lifecycle: pipes, bounds, cancellation and tree cleanup.

use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ego_core::permit::{
    ProcessError, SpawnSpec, StreamProvenance, ToolContentBlock, ToolResult,
};
use ego_core::store::evidence::{EvidenceError, EvidenceStore as _, EvidenceWriter};
use ego_core::turn::CancelToken;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::store::SessionEvidenceStore;

use super::backend::{Backend, executable, prefixed_command};
use super::ExecLimits;

const PIPE_CHUNK_BYTES: usize = 16 * 1024;
const PIPE_CHANNEL_CHUNKS: usize = 4;

pub(crate) async fn execute(
    backend: Arc<dyn Backend>,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    limits: ExecLimits,
    spec: &SpawnSpec,
) -> Result<ToolResult, ProcessError> {
    let mut command = prefixed_command(backend.launch_prefix(), spec)?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut command);

    let stdout_writer = evidence.begin().map_err(ProcessError::Evidence)?;
    let stderr_writer = match evidence.begin() {
        Ok(writer) => writer,
        Err(error) => {
            let _ = abort_writer(stdout_writer).await;
            return Err(ProcessError::Evidence(error));
        }
    };

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let launch = ProcessError::Launch {
                executable: executable(spec),
                reason: error.to_string(),
            };
            let _ = abort_writers([stdout_writer, stderr_writer]).await;
            return Err(launch);
        }
    };
    let tree_id = child.id();
    // An EOF pipe gives the child null input without handing a sandboxed
    // helper a descriptor for a filesystem device.
    drop(child.stdin.take());
    let Some(stdout) = child.stdout.take() else {
        terminate_tree(&mut child, tree_id, limits.termination_grace()).await;
        let _ = abort_writers([stdout_writer, stderr_writer]).await;
        return Err(ProcessError::Invariant(
            "stdout pipe was not installed".into(),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_tree(&mut child, tree_id, limits.termination_grace()).await;
        let _ = abort_writers([stdout_writer, stderr_writer]).await;
        return Err(ProcessError::Invariant(
            "stderr pipe was not installed".into(),
        ));
    };

    let mut stdout_task = Some(start_drain(
        stdout,
        StreamProvenance::Stdout,
        stdout_writer,
        limits.preview_bytes(),
    ));
    let mut stderr_task = Some(start_drain(
        stderr,
        StreamProvenance::Stderr,
        stderr_writer,
        limits.preview_bytes(),
    ));
    let started = Instant::now();
    let mut status = None;
    let mut descendants_cleaned = false;
    let mut stdout_pending = None;
    let mut stderr_pending = None;

    loop {
        if cancel.is_cancelled() {
            request_abort(&mut stdout_task);
            request_abort(&mut stderr_task);
            terminate_tree(&mut child, tree_id, limits.termination_grace()).await;
            let _ = abort_outputs(
                stdout_task.take(),
                stderr_task.take(),
                stdout_pending.take(),
                stderr_pending.take(),
            )
            .await;
            return Err(ProcessError::Cancelled);
        }
        if spec
            .timeout()
            .is_some_and(|timeout| started.elapsed() >= timeout)
        {
            let timeout = spec.timeout().expect("checked as present");
            request_abort(&mut stdout_task);
            request_abort(&mut stderr_task);
            terminate_tree(&mut child, tree_id, limits.termination_grace()).await;
            let _ = abort_outputs(
                stdout_task.take(),
                stderr_task.take(),
                stdout_pending.take(),
                stderr_pending.take(),
            )
            .await;
            return Err(ProcessError::Timeout { timeout });
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(observed) => status = observed,
                Err(error) => {
                    request_abort(&mut stdout_task);
                    request_abort(&mut stderr_task);
                    terminate_tree(&mut child, tree_id, limits.termination_grace()).await;
                    let _ = abort_outputs(
                        stdout_task.take(),
                        stderr_task.take(),
                        stdout_pending.take(),
                        stderr_pending.take(),
                    )
                    .await;
                    return Err(ProcessError::Backend(error.to_string()));
                }
            }
        }
        if status.is_some() && !descendants_cleaned {
            cleanup_descendants(tree_id, limits.termination_grace()).await;
            descendants_cleaned = true;
        }
        if stdout_task
            .as_ref()
            .is_some_and(DrainHandle::is_finished)
        {
            match join_drain(stdout_task.take().expect("present")).await {
                Ok(pending) => stdout_pending = Some(pending),
                Err(error) => {
                    request_abort(&mut stderr_task);
                    terminate_tree(&mut child, tree_id, limits.termination_grace()).await;
                    let _ = abort_outputs(
                        None,
                        stderr_task.take(),
                        stdout_pending.take(),
                        stderr_pending.take(),
                    )
                    .await;
                    return Err(error);
                }
            }
        }
        if stderr_task
            .as_ref()
            .is_some_and(DrainHandle::is_finished)
        {
            match join_drain(stderr_task.take().expect("present")).await {
                Ok(pending) => stderr_pending = Some(pending),
                Err(error) => {
                    request_abort(&mut stdout_task);
                    terminate_tree(&mut child, tree_id, limits.termination_grace()).await;
                    let _ = abort_outputs(
                        stdout_task.take(),
                        None,
                        stdout_pending.take(),
                        stderr_pending.take(),
                    )
                    .await;
                    return Err(error);
                }
            }
        }
        if status.is_some() && stdout_pending.is_some() && stderr_pending.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let stdout = match commit_stream(stdout_pending.expect("stdout reached EOF")).await {
        Ok(block) => block,
        Err(error) => {
            let _ = abort_pending(stderr_pending.expect("stderr reached EOF")).await;
            return Err(error);
        }
    };
    let stderr = commit_stream(stderr_pending.expect("stderr reached EOF")).await?;
    let result = ToolResult::new(vec![stdout, stderr]);
    let status = status.expect("loop exits only with process status");
    if !status.success() {
        return Err(status_error(status, result));
    }

    Ok(result)
}

struct DrainHandle {
    task: JoinHandle<Result<PendingStream, DrainFailure>>,
    abort: Option<oneshot::Sender<()>>,
}

impl DrainHandle {
    fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    fn request_abort(&mut self) {
        if let Some(abort) = self.abort.take() {
            let _ = abort.send(());
        }
    }
}

struct PendingStream {
    stream: StreamProvenance,
    writer: Box<dyn EvidenceWriter>,
    preview: BoundedPreview,
}

enum DrainFailure {
    Aborted,
    Process(ProcessError),
}

enum WriterMessage {
    Chunk(Vec<u8>),
    Finish,
}

fn start_drain(
    pipe: impl AsyncRead + Unpin + Send + 'static,
    stream: StreamProvenance,
    writer: Box<dyn EvidenceWriter>,
    preview_bytes: usize,
) -> DrainHandle {
    let (abort, cancelled) = oneshot::channel();
    DrainHandle {
        task: tokio::spawn(drain(pipe, stream, writer, preview_bytes, cancelled)),
        abort: Some(abort),
    }
}

async fn drain(
    mut pipe: impl AsyncRead + Unpin,
    stream: StreamProvenance,
    writer: Box<dyn EvidenceWriter>,
    preview_bytes: usize,
    mut cancelled: oneshot::Receiver<()>,
) -> Result<PendingStream, DrainFailure> {
    let (sender, receiver) = mpsc::channel(PIPE_CHANNEL_CHUNKS);
    let mut worker = tokio::task::spawn_blocking(move || {
        write_stream(receiver, stream, writer, preview_bytes)
    });
    let mut chunk = [0_u8; PIPE_CHUNK_BYTES];
    loop {
        let read = tokio::select! {
            result = &mut worker => return joined_worker(result),
            _ = &mut cancelled => {
                drop(sender);
                return joined_worker(worker.await);
            }
            result = pipe.read(&mut chunk) => match result {
                Ok(read) => read,
                Err(error) => {
                    drop(sender);
                    return after_pipe_failure(worker.await, error.to_string());
                }
            },
        };
        if read == 0 {
            let mut send = Box::pin(sender.send(WriterMessage::Finish));
            tokio::select! {
                result = &mut worker => return joined_worker(result),
                _ = &mut cancelled => {
                    drop(send);
                    drop(sender);
                    return joined_worker(worker.await);
                }
                result = &mut send => {
                    if result.is_err() {
                        return joined_worker(worker.await);
                    }
                }
            }
            drop(send);
            drop(sender);
            return joined_worker(worker.await);
        }
        let mut send = Box::pin(sender.send(WriterMessage::Chunk(chunk[..read].to_vec())));
        tokio::select! {
            result = &mut worker => return joined_worker(result),
            _ = &mut cancelled => {
                drop(send);
                drop(sender);
                return joined_worker(worker.await);
            }
            result = &mut send => {
                if result.is_err() {
                    return joined_worker(worker.await);
                }
            }
        }
    }
}

async fn join_drain(
    handle: DrainHandle,
) -> Result<PendingStream, ProcessError> {
    match handle.task.await {
        Ok(Ok(pending)) => Ok(pending),
        Ok(Err(DrainFailure::Process(error))) => Err(error),
        Ok(Err(DrainFailure::Aborted)) => Err(ProcessError::Invariant(
            "an output drain aborted without a lifecycle failure".into(),
        )),
        Err(error) => Err(ProcessError::Backend(format!(
            "output drain task failed: {error}"
        ))),
    }
}

fn write_stream(
    mut receiver: mpsc::Receiver<WriterMessage>,
    stream: StreamProvenance,
    mut writer: Box<dyn EvidenceWriter>,
    preview_bytes: usize,
) -> Result<PendingStream, DrainFailure> {
    let mut preview = BoundedPreview::new(preview_bytes);
    while let Some(message) = receiver.blocking_recv() {
        match message {
            WriterMessage::Chunk(chunk) => {
                if let Err(error) = writer.append(&chunk) {
                    let process = evidence_error(stream, error);
                    let _ = writer.abort();
                    return Err(DrainFailure::Process(process));
                }
                preview.append(&chunk);
            }
            WriterMessage::Finish => {
                return Ok(PendingStream {
                    stream,
                    writer,
                    preview,
                });
            }
        }
    }
    match writer.abort() {
        Ok(()) => Err(DrainFailure::Aborted),
        Err(error) => Err(DrainFailure::Process(ProcessError::Evidence(error))),
    }
}

fn joined_worker(
    result: Result<Result<PendingStream, DrainFailure>, tokio::task::JoinError>,
) -> Result<PendingStream, DrainFailure> {
    result.map_err(|error| {
        DrainFailure::Process(ProcessError::Backend(format!(
            "evidence writer task failed: {error}"
        )))
    })?
}

fn after_pipe_failure(
    result: Result<Result<PendingStream, DrainFailure>, tokio::task::JoinError>,
    reason: String,
) -> Result<PendingStream, DrainFailure> {
    match joined_worker(result) {
        Err(DrainFailure::Aborted) => Err(DrainFailure::Process(ProcessError::Backend(reason))),
        other => other,
    }
}

fn request_abort(handle: &mut Option<DrainHandle>) {
    if let Some(handle) = handle {
        handle.request_abort();
    }
}

async fn abort_outputs(
    mut stdout: Option<DrainHandle>,
    mut stderr: Option<DrainHandle>,
    stdout_pending: Option<PendingStream>,
    stderr_pending: Option<PendingStream>,
) -> Result<(), ProcessError> {
    request_abort(&mut stdout);
    request_abort(&mut stderr);
    let mut first_error = None;
    for handle in [stdout, stderr].into_iter().flatten() {
        match handle.task.await {
            Ok(Err(DrainFailure::Aborted)) => {}
            Ok(Ok(pending)) => record_error(&mut first_error, abort_pending(pending).await),
            Ok(Err(DrainFailure::Process(error))) => {
                record_error(&mut first_error, Err(error));
            }
            Err(error) => record_error(
                &mut first_error,
                Err(ProcessError::Backend(format!(
                    "output drain task failed: {error}"
                ))),
            ),
        }
    }
    for pending in [stdout_pending, stderr_pending].into_iter().flatten() {
        record_error(&mut first_error, abort_pending(pending).await);
    }
    first_error.map_or(Ok(()), Err)
}

fn record_error(slot: &mut Option<ProcessError>, result: Result<(), ProcessError>) {
    if slot.is_none() && let Err(error) = result {
        *slot = Some(error);
    }
}

async fn abort_writers(
    writers: [Box<dyn EvidenceWriter>; 2],
) -> Result<(), ProcessError> {
    let mut first_error = None;
    for writer in writers {
        record_error(&mut first_error, abort_writer(writer).await);
    }
    first_error.map_or(Ok(()), Err)
}

async fn abort_pending(pending: PendingStream) -> Result<(), ProcessError> {
    abort_writer(pending.writer).await
}

async fn abort_writer(writer: Box<dyn EvidenceWriter>) -> Result<(), ProcessError> {
    tokio::task::spawn_blocking(move || writer.abort())
        .await
        .map_err(|error| ProcessError::Backend(format!("evidence abort task failed: {error}")))?
        .map_err(ProcessError::Evidence)
}

async fn commit_stream(pending: PendingStream) -> Result<ToolContentBlock, ProcessError> {
    let PendingStream {
        stream,
        writer,
        preview,
    } = pending;
    let reference = tokio::task::spawn_blocking(move || writer.commit())
        .await
        .map_err(|error| ProcessError::Backend(format!("evidence commit task failed: {error}")))?
        .map_err(ProcessError::Evidence)?;
    if reference.bytes != preview.total {
        return Err(ProcessError::Invariant(format!(
            "{stream} evidence committed {} bytes after draining {}",
            reference.bytes, preview.total
        )));
    }
    let (head, tail) = preview.into_parts();
    Ok(ToolContentBlock::process_preview(
        stream, &head, &tail, reference,
    ))
}

fn evidence_error(stream: StreamProvenance, error: EvidenceError) -> ProcessError {
    match error {
        EvidenceError::Limit { limit } => ProcessError::EvidenceLimit { stream, limit },
        error => ProcessError::Evidence(error),
    }
}

struct BoundedPreview {
    budget: usize,
    total: u64,
    small: Option<Vec<u8>>,
    head: Vec<u8>,
    tail: Vec<u8>,
}

impl BoundedPreview {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            total: 0,
            small: Some(Vec::with_capacity(budget)),
            head: Vec::with_capacity(budget / 2),
            tail: Vec::with_capacity(budget.saturating_sub(budget / 2)),
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        if let Some(small) = &mut self.small
            && small.len().saturating_add(bytes.len()) <= self.budget
        {
            small.extend_from_slice(bytes);
            return;
        }
        if let Some(small) = self.small.take() {
            self.append_large(&small);
        }
        self.append_large(bytes);
    }

    fn append_large(&mut self, bytes: &[u8]) {
        let head_budget = self.budget / 2;
        let head_bytes = head_budget
            .saturating_sub(self.head.len())
            .min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_bytes]);
        self.append_tail(&bytes[head_bytes..]);
    }

    fn append_tail(&mut self, bytes: &[u8]) {
        let tail_budget = self.budget.saturating_sub(self.budget / 2);
        if tail_budget == 0 {
            return;
        }
        if bytes.len() >= tail_budget {
            self.tail.clear();
            self.tail
                .extend_from_slice(&bytes[bytes.len().saturating_sub(tail_budget)..]);
            return;
        }
        let excess = self
            .tail
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(tail_budget);
        if excess != 0 {
            self.tail.drain(..excess);
        }
        self.tail.extend_from_slice(bytes);
    }

    fn into_parts(self) -> (Vec<u8>, Vec<u8>) {
        match self.small {
            Some(bytes) => (bytes, Vec::new()),
            None => (self.head, self.tail),
        }
    }
}

fn status_error(status: ExitStatus, result: ToolResult) -> ProcessError {
    if let Some(code) = status.code() {
        return ProcessError::Exit {
            code: Some(code),
            result,
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        ProcessError::Signal {
            signal: status.signal(),
            result,
        }
    }
    #[cfg(not(unix))]
    {
        ProcessError::Signal {
            signal: None,
            result,
        }
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut tokio::process::Command) {
    command.process_group(0);
}

#[cfg(windows)]
fn configure_process_group(command: &mut tokio::process::Command) {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn configure_process_group(_command: &mut tokio::process::Command) {}

async fn terminate_tree(child: &mut Child, tree_id: Option<u32>, grace: Duration) {
    let group_exists = signal_tree(tree_id, false).await;
    if group_exists {
        tokio::time::sleep(grace).await;
        let _ = signal_tree(tree_id, true).await;
    }
    let _ = child.wait().await;
}

async fn cleanup_descendants(tree_id: Option<u32>, grace: Duration) {
    if signal_tree(tree_id, false).await {
        tokio::time::sleep(grace).await;
        let _ = signal_tree(tree_id, true).await;
    }
}

#[cfg(unix)]
async fn signal_tree(tree_id: Option<u32>, force: bool) -> bool {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    let Some(id) = tree_id else {
        return false;
    };
    let Ok(id) = i32::try_from(id) else {
        return false;
    };
    let signal = if force {
        Signal::SIGKILL
    } else {
        Signal::SIGTERM
    };
    killpg(Pid::from_raw(id), signal).is_ok()
}

#[cfg(windows)]
async fn signal_tree(tree_id: Option<u32>, force: bool) -> bool {
    let Some(id) = tree_id else {
        return false;
    };
    let mut command = tokio::process::Command::new("taskkill");
    command.args(["/PID", &id.to_string(), "/T"]);
    if force {
        command.arg("/F");
    }
    command.status().await.is_ok_and(|status| status.success())
}

#[cfg(not(any(unix, windows)))]
async fn signal_tree(_tree_id: Option<u32>, _force: bool) -> bool {
    false
}
