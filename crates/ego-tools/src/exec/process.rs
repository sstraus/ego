//! Common process lifecycle: pipes, bounds, cancellation and tree cleanup.

use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::hook::EffectScope;
use ego_core::permit::{
    CleanupCertainty, DuplexEnd, DuplexSession, DuplexSettlement, ExecCaps, ProcessError,
    RunConfinement, SpawnSpec, StreamProvenance, ToolContentBlock, ToolResult, TransportError,
    TransportFailure,
};
use ego_core::store::evidence::{EvidenceError, EvidenceStore as _, EvidenceWriter};
use ego_core::turn::CancelToken;
use futures::future::BoxFuture;
use futures::io::{AsyncRead as FuturesAsyncRead, AsyncWrite as FuturesAsyncWrite};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::store::SessionEvidenceStore;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use super::ExecLimits;
use super::backend::{Backend, executable, prefixed_command};
use super::process_tree::{
    cleanup_descendants, configure_process_group, terminate_group, terminate_tree,
};

const PIPE_CHUNK_BYTES: usize = 16 * 1024;
const PIPE_CHANNEL_CHUNKS: usize = 4;

/// Process state captured once and shared by the run's normal and duplex
/// process adapters.
pub(crate) struct RunProcessContext {
    backend: Arc<dyn Backend>,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    limits: ExecLimits,
    confinement: RunConfinement,
    capabilities: ExecCaps,
}

impl RunProcessContext {
    pub(crate) fn new(
        backend: Arc<dyn Backend>,
        evidence: SessionEvidenceStore,
        cancel: CancelToken,
        limits: ExecLimits,
        confinement: RunConfinement,
        capabilities: ExecCaps,
    ) -> Self {
        Self {
            backend,
            evidence,
            cancel,
            limits,
            confinement,
            capabilities,
        }
    }

    pub(crate) fn offline(&self) -> bool {
        self.confinement.offline()
    }

    pub(crate) async fn execute(&self, spec: &SpawnSpec) -> Result<ToolResult, ProcessError> {
        self.execute_inner(spec, None).await
    }

    pub(crate) async fn execute_handler(
        &self,
        spec: &SpawnSpec,
        input: &[u8],
        scope: EffectScope,
    ) -> Result<ToolResult, ProcessError> {
        if scope.is_cancelled() {
            return Err(ProcessError::Cancelled);
        }
        if Instant::now() >= scope.deadline() {
            return Err(ProcessError::Timeout {
                timeout: scope.timeout(),
            });
        }
        self.execute_inner(spec, Some((input.to_vec(), scope)))
            .await
    }

    async fn execute_inner(
        &self,
        spec: &SpawnSpec,
        handler: Option<(Vec<u8>, EffectScope)>,
    ) -> Result<ToolResult, ProcessError> {
        self.confinement
            .requirements()
            .check(self.capabilities)
            .map_err(ProcessError::MissingCapabilities)?;
        let backend = Arc::clone(&self.backend);
        let evidence = self.evidence.clone();
        let cancel = self.cancel.clone();
        let limits = self.limits;
        let spec = spec.clone();
        super::runtime_handle()
            .spawn(async move { execute(backend, evidence, cancel, limits, &spec, handler).await })
            .await
            .map_err(|error| ProcessError::Backend(format!("process task failed: {error}")))?
    }

    pub(crate) async fn spawn_duplex(
        self: Arc<Self>,
        spec: SpawnSpec,
    ) -> Result<Box<dyn DuplexSession>, TransportError> {
        super::runtime_handle()
            .spawn(async move { spawn_duplex(self, spec).await })
            .await
            .map_err(|error| {
                TransportError::definitely_unsent(TransportFailure::Exchange(format!(
                    "duplex spawn task failed: {error}"
                )))
            })?
    }
}

pub(crate) async fn execute(
    backend: Arc<dyn Backend>,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    limits: ExecLimits,
    spec: &SpawnSpec,
    handler: Option<(Vec<u8>, EffectScope)>,
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
    let stdin = child.stdin.take();
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
    if let Some((input, scope)) = &handler {
        let Some(mut stdin) = stdin else {
            terminate_tree(&mut child, tree_id, limits.termination_grace()).await;
            let _ = abort_outputs(stdout_task.take(), stderr_task.take(), None, None).await;
            return Err(ProcessError::Invariant(
                "stdin pipe was not installed".into(),
            ));
        };
        if let Err(error) = write_handler_input(&mut stdin, input, &cancel, scope).await {
            terminate_tree(&mut child, tree_id, limits.termination_grace()).await;
            let _ = abort_outputs(stdout_task.take(), stderr_task.take(), None, None).await;
            return Err(error);
        }
    } else {
        // An EOF pipe gives the child null input without handing a sandboxed
        // helper a descriptor for a filesystem device.
        drop(stdin);
    }
    let started = Instant::now();
    let mut status = None;
    let mut descendants_cleaned = false;
    let mut stdout_pending = None;
    let mut stderr_pending = None;

    loop {
        if cancel.is_cancelled()
            || handler
                .as_ref()
                .is_some_and(|(_, scope)| scope.is_cancelled())
        {
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
        if let Some((_, scope)) = &handler
            && Instant::now() >= scope.deadline()
        {
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
            return Err(ProcessError::Timeout {
                timeout: scope.timeout(),
            });
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
        if stdout_task.as_ref().is_some_and(DrainHandle::is_finished) {
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
        if stderr_task.as_ref().is_some_and(DrainHandle::is_finished) {
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

async fn write_handler_input(
    stdin: &mut tokio::process::ChildStdin,
    input: &[u8],
    parent_cancel: &CancelToken,
    scope: &EffectScope,
) -> Result<(), ProcessError> {
    let mut write = Box::pin(async {
        stdin.write_all(input).await?;
        stdin.shutdown().await
    });
    loop {
        tokio::select! {
            result = &mut write => {
                return result.map_err(|error| ProcessError::Backend(
                    format!("writing hook handler stdin failed: {error}")
                ));
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(scope.deadline())) => {
                return Err(ProcessError::Timeout { timeout: scope.timeout() });
            }
            _ = tokio::time::sleep(Duration::from_millis(2)) => {
                if parent_cancel.is_cancelled() || scope.is_cancelled() {
                    return Err(ProcessError::Cancelled);
                }
            }
        }
    }
}

async fn spawn_duplex(
    context: Arc<RunProcessContext>,
    spec: SpawnSpec,
) -> Result<Box<dyn DuplexSession>, TransportError> {
    if let Err(missing) = context
        .confinement
        .requirements()
        .check(context.capabilities)
    {
        let failure = if missing.missing().contains(ExecCaps::NO_IP_NETWORK) {
            TransportFailure::Offline
        } else {
            TransportFailure::Exchange(missing.to_string())
        };
        return Err(TransportError::definitely_unsent(failure));
    }
    if context.cancel.is_cancelled() {
        return Err(TransportError::definitely_unsent(
            TransportFailure::Cancelled,
        ));
    }

    let mut command =
        prefixed_command(context.backend.launch_prefix(), &spec).map_err(process_before_spawn)?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut command);

    let stderr_writer = context
        .evidence
        .begin()
        .map_err(|error| process_before_spawn(ProcessError::Evidence(error)))?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = abort_writer(stderr_writer).await;
            return Err(process_before_spawn(ProcessError::Launch {
                executable: executable(&spec),
                reason: error.to_string(),
            }));
        }
    };
    let tree_id = child.id();
    let Some(stdin) = child.stdin.take() else {
        terminate_tree(&mut child, tree_id, context.limits.termination_grace()).await;
        let _ = abort_writer(stderr_writer).await;
        return Err(process_before_spawn(ProcessError::Invariant(
            "stdin pipe was not installed".into(),
        )));
    };
    let Some(stdout) = child.stdout.take() else {
        terminate_tree(&mut child, tree_id, context.limits.termination_grace()).await;
        let _ = abort_writer(stderr_writer).await;
        return Err(process_before_spawn(ProcessError::Invariant(
            "stdout pipe was not installed".into(),
        )));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_tree(&mut child, tree_id, context.limits.termination_grace()).await;
        let _ = abort_writer(stderr_writer).await;
        return Err(process_before_spawn(ProcessError::Invariant(
            "stderr pipe was not installed".into(),
        )));
    };

    let delivery = Arc::new(AtomicBool::new(false));
    let started = tokio::time::Instant::now();
    Ok(Box::new(ProcessDuplex {
        stdin: Some(stdin.compat_write()),
        stdout: Some(stdout.compat()),
        lifecycle: Some(DuplexLifecycle {
            child,
            tree_id,
            stderr: start_drain(
                stderr,
                StreamProvenance::Stderr,
                stderr_writer,
                context.limits.preview_bytes(),
            ),
            watcher: Some(start_duplex_watcher(
                tree_id,
                context.cancel.clone(),
                spec.timeout(),
                started,
                context.limits.termination_grace(),
            )),
            started,
            timeout: spec.timeout(),
            cancel: context.cancel.clone(),
            grace: context.limits.termination_grace(),
        }),
        delivery,
        _context: context,
    }))
}

fn process_before_spawn(error: ProcessError) -> TransportError {
    TransportError::definitely_unsent(TransportFailure::Exchange(error.to_string()))
}

struct DuplexLifecycle {
    child: Child,
    tree_id: Option<u32>,
    stderr: DrainHandle,
    watcher: Option<DuplexWatcher>,
    started: tokio::time::Instant,
    timeout: Option<Duration>,
    cancel: CancelToken,
    grace: Duration,
}

struct ProcessDuplex {
    stdin: Option<Compat<tokio::process::ChildStdin>>,
    stdout: Option<Compat<tokio::process::ChildStdout>>,
    lifecycle: Option<DuplexLifecycle>,
    delivery: Arc<AtomicBool>,
    _context: Arc<RunProcessContext>,
}

impl FuturesAsyncRead for ProcessDuplex {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let Some(stdout) = self.stdout.as_mut() else {
            return Poll::Ready(Ok(0));
        };
        Pin::new(stdout).poll_read(context, buffer)
    }
}

impl FuturesAsyncWrite for ProcessDuplex {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "duplex stdin is closed",
            )));
        };
        match Pin::new(stdin).poll_write(context, buffer) {
            Poll::Ready(Ok(written)) => {
                if written != 0 {
                    self.delivery.store(true, Ordering::Release);
                }
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.stdin.as_mut() {
            Some(stdin) => Pin::new(stdin).poll_flush(context),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.stdin.as_mut() {
            Some(stdin) => Pin::new(stdin).poll_close(context),
            None => Poll::Ready(Ok(())),
        }
    }
}

impl DuplexSession for ProcessDuplex {
    fn delivery(&self) -> DeliveryCertainty {
        delivery_certainty(&self.delivery)
    }

    fn settle(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement> {
        settle_on_runtime(*self, false)
    }

    fn interrupt(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement> {
        settle_on_runtime(*self, true)
    }
}

fn settle_on_runtime(
    session: ProcessDuplex,
    interrupt: bool,
) -> BoxFuture<'static, DuplexSettlement> {
    let delivery = Arc::clone(&session.delivery);
    let task = super::runtime_handle().spawn(settle_duplex(session, interrupt));
    Box::pin(async move {
        task.await.unwrap_or_else(|error| {
            DuplexSettlement::new(
                DuplexEnd::Failed,
                CleanupCertainty::Uncertain,
                delivery_certainty(&delivery),
                Err(EvidenceError::Io {
                    message: format!("duplex settlement task failed: {error}"),
                }),
            )
        })
    })
}

async fn settle_duplex(mut session: ProcessDuplex, interrupt: bool) -> DuplexSettlement {
    drop(session.stdin.take());
    drop(session.stdout.take());
    let delivery = delivery_certainty(&session.delivery);
    let Some(mut lifecycle) = session.lifecycle.take() else {
        return DuplexSettlement::new(
            DuplexEnd::Failed,
            CleanupCertainty::Uncertain,
            delivery,
            Err(EvidenceError::Io {
                message: "duplex lifecycle was already consumed".into(),
            }),
        );
    };

    let watcher_complete = match lifecycle.watcher.take() {
        Some(watcher) => watcher.stop().await,
        None => false,
    };

    let (end, mut cleanup) = if interrupt {
        let complete =
            terminate_tree(&mut lifecycle.child, lifecycle.tree_id, lifecycle.grace).await;
        (DuplexEnd::Interrupted, cleanup_certainty(complete))
    } else {
        settle_child(&mut lifecycle).await
    };
    if !watcher_complete {
        cleanup = CleanupCertainty::Uncertain;
    }
    let evidence = finalize_stderr(lifecycle.stderr).await;
    DuplexSettlement::new(end, cleanup, delivery, evidence)
}

async fn settle_child(lifecycle: &mut DuplexLifecycle) -> (DuplexEnd, CleanupCertainty) {
    loop {
        if lifecycle.cancel.is_cancelled() {
            let complete =
                terminate_tree(&mut lifecycle.child, lifecycle.tree_id, lifecycle.grace).await;
            return (DuplexEnd::Cancelled, cleanup_certainty(complete));
        }
        if lifecycle
            .timeout
            .is_some_and(|timeout| lifecycle.started.elapsed() >= timeout)
        {
            let timeout = lifecycle.timeout.expect("checked as present");
            let complete =
                terminate_tree(&mut lifecycle.child, lifecycle.tree_id, lifecycle.grace).await;
            return (DuplexEnd::TimedOut { timeout }, cleanup_certainty(complete));
        }
        match lifecycle.child.try_wait() {
            Ok(Some(status)) => {
                let complete = cleanup_descendants(lifecycle.tree_id, lifecycle.grace).await;
                return (duplex_end(status), cleanup_certainty(complete));
            }
            Ok(None) => tokio::time::sleep(Duration::from_millis(2)).await,
            Err(_) => {
                terminate_tree(&mut lifecycle.child, lifecycle.tree_id, lifecycle.grace).await;
                return (DuplexEnd::Failed, CleanupCertainty::Uncertain);
            }
        }
    }
}

fn duplex_end(status: ExitStatus) -> DuplexEnd {
    if let Some(code) = status.code() {
        return DuplexEnd::Exited { code };
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        DuplexEnd::Signalled {
            signal: status.signal(),
        }
    }
    #[cfg(not(unix))]
    {
        DuplexEnd::Signalled { signal: None }
    }
}

fn cleanup_certainty(complete: bool) -> CleanupCertainty {
    if complete {
        CleanupCertainty::Complete
    } else {
        CleanupCertainty::Uncertain
    }
}

struct DuplexWatcher {
    task: JoinHandle<()>,
    stop: oneshot::Sender<()>,
}

impl DuplexWatcher {
    async fn stop(self) -> bool {
        let Self { task, stop } = self;
        let _ = stop.send(());
        task.await.is_ok()
    }
}

fn start_duplex_watcher(
    tree_id: Option<u32>,
    cancel: CancelToken,
    timeout: Option<Duration>,
    started: tokio::time::Instant,
    grace: Duration,
) -> DuplexWatcher {
    let (stop, mut stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            if cancel.is_cancelled() || timeout.is_some_and(|limit| started.elapsed() >= limit) {
                terminate_group(tree_id, grace).await;
                return;
            }
            tokio::select! {
                _ = &mut stopped => return,
                _ = tokio::time::sleep(Duration::from_millis(2)) => {}
            }
        }
    });
    DuplexWatcher { task, stop }
}

async fn finalize_stderr(handle: DrainHandle) -> Result<ToolResult, EvidenceError> {
    let pending = join_drain(handle).await.map_err(process_to_evidence)?;
    let block = commit_stream(pending).await.map_err(process_to_evidence)?;
    Ok(ToolResult::new(vec![block]))
}

fn process_to_evidence(error: ProcessError) -> EvidenceError {
    match error {
        ProcessError::Evidence(error) => error,
        ProcessError::EvidenceLimit { limit, .. } => EvidenceError::Limit { limit },
        error => EvidenceError::Io {
            message: error.to_string(),
        },
    }
}

fn delivery_certainty(delivery: &AtomicBool) -> DeliveryCertainty {
    if delivery.load(Ordering::Acquire) {
        DeliveryCertainty::PossiblySent
    } else {
        DeliveryCertainty::DefinitelyUnsent
    }
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
    let mut worker =
        tokio::task::spawn_blocking(move || write_stream(receiver, stream, writer, preview_bytes));
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

async fn join_drain(handle: DrainHandle) -> Result<PendingStream, ProcessError> {
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
    if slot.is_none()
        && let Err(error) = result
    {
        *slot = Some(error);
    }
}

async fn abort_writers(writers: [Box<dyn EvidenceWriter>; 2]) -> Result<(), ProcessError> {
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
        let head_bytes = head_budget.saturating_sub(self.head.len()).min(bytes.len());
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
