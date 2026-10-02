//! Common process lifecycle: pipes, bounds, cancellation and tree cleanup.

use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::hook::EffectScope;
use ego_core::permit::{
    CleanupCertainty, DuplexEnd, DuplexSession, DuplexSettlement, ExecCaps, ProcessError,
    RunConfinement, SandboxGrant, SandboxGrantPort, SpawnSpec, StreamProvenance, ToolContentBlock,
    ToolResult, TransportError, TransportFailure,
};
use ego_core::store::evidence::{EvidenceError, EvidenceStore as _, EvidenceWriter};
use ego_core::turn::CancelToken;
use futures::future::BoxFuture;
use futures::io::{AsyncRead as FuturesAsyncRead, AsyncWrite as FuturesAsyncWrite};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::store::SessionEvidenceStore;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use super::ExecLimits;
use super::backend::{Backend, executable, prefixed_command};
use super::denial;
use super::process_tree::ManagedChild;

/// Questions one command may put to the human before its refusal is final.
/// Each answer can reveal one more path, so the bound is a few paths, not one.
const MAX_SANDBOX_QUESTIONS: usize = 3;

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
    sandbox_grants: Option<Arc<dyn SandboxGrantPort>>,
    /// The last profile rendered for a grant list, so a run of commands under
    /// the same grants shares one private profile file.
    widened: Mutex<Option<(Vec<SandboxGrant>, Arc<dyn Backend>)>>,
}

impl RunProcessContext {
    pub(crate) fn new(
        backend: Arc<dyn Backend>,
        evidence: SessionEvidenceStore,
        cancel: CancelToken,
        limits: ExecLimits,
        confinement: RunConfinement,
        capabilities: ExecCaps,
        sandbox_grants: Option<Arc<dyn SandboxGrantPort>>,
    ) -> Self {
        Self {
            backend,
            evidence,
            cancel,
            limits,
            confinement,
            capabilities,
            sandbox_grants,
            widened: Mutex::new(None),
        }
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
        match (&self.sandbox_grants, &handler) {
            (Some(grants), None) => self.execute_asking(grants.as_ref(), spec).await,
            _ => {
                self.spawn_once(Arc::clone(&self.backend), spec, handler)
                    .await
            }
        }
    }

    async fn spawn_once(
        &self,
        backend: Arc<dyn Backend>,
        spec: &SpawnSpec,
        handler: Option<(Vec<u8>, EffectScope)>,
    ) -> Result<ToolResult, ProcessError> {
        let evidence = self.evidence.clone();
        let cancel = self.cancel.clone();
        let limits = self.limits;
        let spec = spec.clone();
        super::runtime_handle()
            .spawn(async move { execute(backend, evidence, cancel, limits, &spec, handler).await })
            .await
            .map_err(|error| ProcessError::Backend(format!("process task failed: {error}")))?
    }

    /// Run a command, and when the sandbox refuses a path it names, ask the
    /// human whether to open that one path and run it again.
    ///
    /// The command is re-run only after it has exited unsuccessfully, so no
    /// process of the first attempt is still alive; whatever it did before the
    /// refusal is not undone, and the human is told which path the retry is
    /// for.
    async fn execute_asking(
        &self,
        grants: &dyn SandboxGrantPort,
        spec: &SpawnSpec,
    ) -> Result<ToolResult, ProcessError> {
        let mut questions = 0;
        loop {
            let granted = grants.granted();
            let backend = self.backend_under(&granted)?;
            let (code, mut result) = match self.spawn_once(backend, spec, None).await {
                Err(ProcessError::Exit { code, result }) => (code, result),
                other => return other,
            };
            let Some(found) = denial::analyze(spec, &result, &self.confinement, &granted) else {
                return Err(ProcessError::Exit { code, result });
            };
            let outcome = match &found.request {
                Some(_) if !self.backend.can_widen() => denial::Outcome::CannotWiden,
                Some(request) if questions < MAX_SANDBOX_QUESTIONS => {
                    questions += 1;
                    match self.ask_until_cancelled(grants, request, spec).await {
                        Some(true) => continue,
                        Some(false) => denial::Outcome::Declined,
                        None => return Err(ProcessError::Cancelled),
                    }
                }
                Some(_) => denial::Outcome::QuestionLimit,
                None => denial::Outcome::NobodyToAsk,
            };
            result.content.push(ToolContentBlock::builtin(denial::note(
                &found,
                &self.confinement,
                outcome,
            )));
            return Err(ProcessError::Exit { code, result });
        }
    }

    /// The person's answer, or `None` when the run was cancelled first: a
    /// question nobody answers must not hold a cancelled run open.
    async fn ask_until_cancelled(
        &self,
        grants: &dyn SandboxGrantPort,
        request: &SandboxGrant,
        spec: &SpawnSpec,
    ) -> Option<bool> {
        let command = denial::command_line(spec);
        let question = grants.ask_for(request, &command);
        tokio::pin!(question);
        loop {
            tokio::select! {
                answer = &mut question => return Some(answer),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    if self.cancel.is_cancelled() {
                        return None;
                    }
                }
            }
        }
    }

    /// The backend with `granted` added, or the admitted one when nothing is
    /// granted or the mechanism cannot express a per-path grant.
    fn backend_under(&self, granted: &[SandboxGrant]) -> Result<Arc<dyn Backend>, ProcessError> {
        // A stored grant the profile cannot express (a hand-edited ledger) is
        // left out, so it cannot make every command of the workspace fail.
        let granted = granted
            .iter()
            .filter(|grant| denial::renderable(grant.path.as_path()))
            .cloned()
            .collect::<Vec<_>>();
        let granted = granted.as_slice();
        if granted.is_empty() {
            return Ok(Arc::clone(&self.backend));
        }
        let mut cache = self
            .widened
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((known, backend)) = cache.as_ref()
            && known.as_slice() == granted
        {
            return Ok(Arc::clone(backend));
        }
        let Some(widened) = self.backend.widened(granted)? else {
            return Ok(Arc::clone(&self.backend));
        };
        *cache = Some((granted.to_vec(), Arc::clone(&widened)));
        Ok(widened)
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
        .stderr(Stdio::piped());

    let stdout_writer = evidence.begin().map_err(ProcessError::Evidence)?;
    let stderr_writer = match evidence.begin() {
        Ok(writer) => writer,
        Err(error) => {
            let _ = abort_writer(stdout_writer).await;
            return Err(ProcessError::Evidence(error));
        }
    };

    if let Err(error) = super::identity::revalidate(spec) {
        let _ = abort_writers([stdout_writer, stderr_writer]).await;
        return Err(error);
    }
    let mut child = match ManagedChild::spawn(command) {
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
    let stdin = child.take_stdin();
    let Some(stdout) = child.take_stdout() else {
        child.terminate(limits.termination_grace()).await;
        let _ = abort_writers([stdout_writer, stderr_writer]).await;
        return Err(ProcessError::Invariant(
            "stdout pipe was not installed".into(),
        ));
    };
    let Some(stderr) = child.take_stderr() else {
        child.terminate(limits.termination_grace()).await;
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
            child.terminate(limits.termination_grace()).await;
            let _ = abort_outputs(stdout_task.take(), stderr_task.take(), None, None).await;
            return Err(ProcessError::Invariant(
                "stdin pipe was not installed".into(),
            ));
        };
        if let Err(error) = write_handler_input(&mut stdin, input, &cancel, scope).await {
            child.terminate(limits.termination_grace()).await;
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
            child.terminate(limits.termination_grace()).await;
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
            child.terminate(limits.termination_grace()).await;
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
            tracing::warn!(
                elapsed_ms = started.elapsed().as_millis(),
                timeout_ms = timeout.as_millis(),
                child_exited = status.is_some(),
                descendants_cleaned,
                stdout_drained = stdout_pending.is_some(),
                stderr_drained = stderr_pending.is_some(),
                "process deadline expired"
            );
            request_abort(&mut stdout_task);
            request_abort(&mut stderr_task);
            child.terminate(limits.termination_grace()).await;
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
                    child.terminate(limits.termination_grace()).await;
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
            child.cleanup_after_exit(limits.termination_grace()).await;
            descendants_cleaned = true;
        }
        if stdout_task.as_ref().is_some_and(DrainHandle::is_finished) {
            match join_drain(stdout_task.take().expect("present")).await {
                Ok(pending) => stdout_pending = Some(pending),
                Err(error) => {
                    request_abort(&mut stderr_task);
                    child.terminate(limits.termination_grace()).await;
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
                    child.terminate(limits.termination_grace()).await;
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
    if !context.confinement.admits_mcp_stdio() {
        return Err(TransportError::definitely_unsent(TransportFailure::Offline));
    }
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
        .stderr(Stdio::piped());

    let stderr_writer = context
        .evidence
        .begin()
        .map_err(|error| process_before_spawn(ProcessError::Evidence(error)))?;
    let mut child = match ManagedChild::spawn(command) {
        Ok(child) => child,
        Err(error) => {
            let _ = abort_writer(stderr_writer).await;
            return Err(process_before_spawn(ProcessError::Launch {
                executable: executable(&spec),
                reason: error.to_string(),
            }));
        }
    };
    let Some(stdin) = child.take_stdin() else {
        child.terminate(context.limits.termination_grace()).await;
        let _ = abort_writer(stderr_writer).await;
        return Err(process_before_spawn(ProcessError::Invariant(
            "stdin pipe was not installed".into(),
        )));
    };
    let Some(stdout) = child.take_stdout() else {
        child.terminate(context.limits.termination_grace()).await;
        let _ = abort_writer(stderr_writer).await;
        return Err(process_before_spawn(ProcessError::Invariant(
            "stdout pipe was not installed".into(),
        )));
    };
    let Some(stderr) = child.take_stderr() else {
        child.terminate(context.limits.termination_grace()).await;
        let _ = abort_writer(stderr_writer).await;
        return Err(process_before_spawn(ProcessError::Invariant(
            "stderr pipe was not installed".into(),
        )));
    };

    let delivery = Arc::new(AtomicBool::new(false));
    let started = tokio::time::Instant::now();
    let supervisor = start_duplex_supervisor(
        child,
        context.cancel.clone(),
        spec.timeout(),
        started,
        context.limits.termination_grace(),
    );
    Ok(Box::new(ProcessDuplex {
        stdin: Some(stdin.compat_write()),
        stdout: Some(stdout.compat()),
        lifecycle: Some(DuplexLifecycle {
            stderr: start_drain(
                stderr,
                StreamProvenance::Stderr,
                stderr_writer,
                context.limits.preview_bytes(),
            ),
            supervisor,
        }),
        delivery,
        _context: context,
    }))
}

fn process_before_spawn(error: ProcessError) -> TransportError {
    TransportError::definitely_unsent(TransportFailure::Exchange(error.to_string()))
}

struct DuplexLifecycle {
    stderr: DrainHandle,
    supervisor: DuplexSupervisor,
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
    let Some(lifecycle) = session.lifecycle.take() else {
        return DuplexSettlement::new(
            DuplexEnd::Failed,
            CleanupCertainty::Uncertain,
            delivery,
            Err(EvidenceError::Io {
                message: "duplex lifecycle was already consumed".into(),
            }),
        );
    };

    let (end, cleanup) = lifecycle.supervisor.finish(interrupt).await;
    let evidence = finalize_stderr(lifecycle.stderr).await;
    DuplexSettlement::new(end, cleanup, delivery, evidence)
}

enum DuplexControl {
    Interrupt,
}

struct DuplexSupervisor {
    control: mpsc::UnboundedSender<DuplexControl>,
    task: JoinHandle<(DuplexEnd, CleanupCertainty)>,
}

impl DuplexSupervisor {
    async fn finish(self, interrupt: bool) -> (DuplexEnd, CleanupCertainty) {
        let Self { control, task } = self;
        if interrupt {
            let _ = control.send(DuplexControl::Interrupt);
        }
        let (end, cleanup) = task
            .await
            .unwrap_or((DuplexEnd::Failed, CleanupCertainty::Uncertain));
        drop(control);
        if interrupt {
            (DuplexEnd::Interrupted, cleanup)
        } else {
            (end, cleanup)
        }
    }
}

fn start_duplex_supervisor(
    child: ManagedChild,
    cancel: CancelToken,
    timeout: Option<Duration>,
    started: tokio::time::Instant,
    grace: Duration,
) -> DuplexSupervisor {
    let (control, receiver) = mpsc::unbounded_channel();
    let task = tokio::spawn(supervise_duplex_child(
        child, receiver, cancel, timeout, started, grace,
    ));
    DuplexSupervisor { control, task }
}

async fn supervise_duplex_child(
    mut child: ManagedChild,
    mut control: mpsc::UnboundedReceiver<DuplexControl>,
    cancel: CancelToken,
    timeout: Option<Duration>,
    started: tokio::time::Instant,
    grace: Duration,
) -> (DuplexEnd, CleanupCertainty) {
    loop {
        if cancel.is_cancelled() {
            let complete = child.terminate(grace).await;
            return (DuplexEnd::Cancelled, cleanup_certainty(complete));
        }
        if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
            let timeout = timeout.expect("checked as present");
            let complete = child.terminate(grace).await;
            return (DuplexEnd::TimedOut { timeout }, cleanup_certainty(complete));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let complete = child.cleanup_after_exit(grace).await;
                return (duplex_end(status), cleanup_certainty(complete));
            }
            Ok(None) => {}
            Err(_) => {
                child.terminate(grace).await;
                return (DuplexEnd::Failed, CleanupCertainty::Uncertain);
            }
        }
        tokio::select! {
            command = control.recv() => {
                let complete = child.terminate(grace).await;
                let end = match command {
                    Some(DuplexControl::Interrupt) => DuplexEnd::Interrupted,
                    None => DuplexEnd::Failed,
                };
                return (end, cleanup_certainty(complete));
            }
            _ = tokio::time::sleep(Duration::from_millis(2)) => {}
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

#[cfg(test)]
mod sandbox_question_tests {
    use std::ffi::OsString;
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ego_core::event::domain::CancelCause;
    use ego_core::event::id::SessionId;
    use ego_core::permit::{
        BackendIdentity, CanonPath, Confinement, EnvPolicy, ExecCaps, ProcessError, RunConfinement,
        SandboxGrant, SandboxGrantPort, SpawnSpec,
    };
    use futures::future::BoxFuture;

    use super::*;
    use crate::exec::backend::LaunchPrefix;
    use crate::exec::null::NullBackend;

    /// Unconfined like [`NullBackend`], but claims it can open one more path.
    struct Widening;

    impl Backend for Widening {
        fn identity(&self) -> BackendIdentity {
            NullBackend.identity()
        }

        fn maximum_demonstrable_caps(&self) -> ExecCaps {
            ExecCaps::null()
        }

        fn launch_prefix(&self) -> LaunchPrefix {
            NullBackend.launch_prefix()
        }

        fn widened(&self, _: &[SandboxGrant]) -> Result<Option<Arc<dyn Backend>>, ProcessError> {
            Ok(Some(Arc::new(NullBackend)))
        }

        fn can_widen(&self) -> bool {
            true
        }
    }

    /// A seat that counts questions and, when `hang`, never answers.
    struct Seat {
        asked: AtomicUsize,
        hang: bool,
    }

    impl SandboxGrantPort for Seat {
        fn granted(&self) -> Vec<SandboxGrant> {
            Vec::new()
        }

        fn ask<'a>(&'a self, _: &'a SandboxGrant) -> BoxFuture<'a, bool> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            if self.hang {
                Box::pin(std::future::pending())
            } else {
                Box::pin(async { true })
            }
        }
    }

    struct Rig {
        context: RunProcessContext,
        cancel: CancelToken,
        seat: Arc<Seat>,
        spec: SpawnSpec,
        _dirs: Vec<tempfile::TempDir>,
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test Tokio runtime")
            .block_on(future)
    }

    /// A run whose child exits 1 printing a refusal for a file outside the
    /// workspace that its own command line names.
    fn rig(backend: Arc<dyn Backend>, hang: bool) -> Rig {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let evidence = tempfile::tempdir().expect("evidence");
        let target = std::fs::canonicalize(outside.path())
            .expect("canonical outside")
            .join("f");
        std::fs::write(&target, "x").expect("outside file");
        let confinement = RunConfinement::new(
            Confinement::Workspace,
            false,
            CanonPath::new(std::fs::canonicalize(workspace.path()).expect("canonical workspace")),
        )
        .expect("confinement");
        let cancel = CancelToken::new();
        let seat = Arc::new(Seat {
            asked: AtomicUsize::new(0),
            hang,
        });
        let context = RunProcessContext::new(
            backend,
            SessionEvidenceStore::open(evidence.path(), SessionId::new(), 1024 * 1024)
                .expect("evidence store"),
            cancel.clone(),
            ExecLimits::new(4096, Duration::from_millis(250)).expect("limits"),
            confinement.clone(),
            ExecCaps::READ_SCOPED
                .union(ExecCaps::WRITE_SCOPED)
                .union(ExecCaps::WRITE_DENIED)
                .union(ExecCaps::NO_IP_NETWORK),
            Some(Arc::clone(&seat) as Arc<dyn SandboxGrantPort>),
        );
        let spec = SpawnSpec::new(
            vec![
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from("printf '%s: Operation not permitted\\n' \"$0\" >&2; exit 1"),
                target.into_os_string(),
            ],
            Some(confinement.workspace().clone()),
            None,
            EnvPolicy::Inherited,
        )
        .expect("spawn spec");
        Rig {
            context,
            cancel,
            seat,
            spec,
            _dirs: vec![workspace, outside, evidence],
        }
    }

    /// Catches: on a backend that cannot widen after admission (Landlock) the
    /// person is asked, answers yes, and is told a grant is in force although
    /// the retry runs under the same confinement.
    #[test]
    fn a_backend_that_cannot_widen_is_never_asked_and_says_so() {
        let rig = rig(Arc::new(NullBackend), false);
        let outcome = block_on(rig.context.execute(&rig.spec));
        let Err(ProcessError::Exit { result, .. }) = outcome else {
            panic!("the forged refusal exits 1: {outcome:?}");
        };
        assert_eq!(rig.seat.asked.load(Ordering::SeqCst), 0);
        let text = result
            .content
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("cannot open extra paths"), "{text}");
    }

    /// Catches: a sandbox question nobody answers holds a cancelled run open
    /// forever, because `ask` is awaited without watching the run's cancel
    /// token.
    #[test]
    fn a_cancelled_run_does_not_wait_for_a_sandbox_answer() {
        let rig = rig(Arc::new(Widening), true);
        let cancel = rig.cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            cancel.cancel(CancelCause::User);
        });
        let outcome = block_on(async {
            tokio::time::timeout(Duration::from_secs(10), rig.context.execute(&rig.spec)).await
        });
        canceller.join().expect("canceller thread");
        assert!(
            matches!(outcome, Ok(Err(ProcessError::Cancelled))),
            "{outcome:?}"
        );
        assert_eq!(rig.seat.asked.load(Ordering::SeqCst), 1);
    }
}
