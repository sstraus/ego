//! Protocol-neutral execution for a secret-bearing child exchange.

use std::ffi::OsString;
use std::fmt;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use ego_core::credentials::SecretMaterial;
use ego_core::permit::CanonPath;
use ego_core::turn::CancelToken;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::task::JoinHandle;
use zeroize::{Zeroize as _, Zeroizing};

use super::process_tree::ManagedChild;

/// Complete authority and lifecycle bounds for one secret child exchange.
pub struct SecretChildRequest {
    pub argv: Vec<OsString>,
    pub cwd: Option<CanonPath>,
    pub stdin: Option<SecretMaterial>,
    pub cancel: CancelToken,
    pub run_deadline: Option<Instant>,
    pub command_timeout: Duration,
    pub termination_grace: Duration,
}

/// Successful, opaque credential-protocol output.
pub struct SecretChildOutput {
    stdout: SecretMaterial,
}

impl SecretChildOutput {
    pub fn stdout(&self) -> &SecretMaterial {
        &self.stdout
    }

    pub fn into_stdout(self) -> SecretMaterial {
        self.stdout
    }
}

impl fmt::Debug for SecretChildOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretChildOutput")
            .field("stdout", &self.stdout)
            .finish()
    }
}

/// Closed, secret-free failure classification for a child exchange.
#[derive(Debug)]
pub enum SecretChildError {
    InvalidRequest(&'static str),
    Launch(std::io::Error),
    Io(std::io::Error),
    Cancelled,
    RunTimeout,
    CommandTimeout,
    Exit(Option<i32>),
    CleanupUncertain {
        cause: Option<Box<SecretChildError>>,
    },
}

impl fmt::Display for SecretChildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(reason) => formatter.write_str(reason),
            Self::Launch(error) => write!(formatter, "could not start credential command: {error}"),
            Self::Io(error) => write!(formatter, "credential command I/O failed: {error}"),
            Self::Cancelled => formatter.write_str("credential command was cancelled"),
            Self::RunTimeout => formatter.write_str("run deadline expired"),
            Self::CommandTimeout => formatter.write_str("credential command timed out"),
            Self::Exit(Some(code)) => {
                write!(formatter, "credential command exited with code {code}")
            }
            Self::Exit(None) => formatter.write_str("credential command exited by signal"),
            Self::CleanupUncertain { cause } => {
                formatter.write_str("credential command process-tree cleanup is uncertain")?;
                if let Some(cause) = cause {
                    write!(formatter, " after {cause}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for SecretChildError {}

enum Settlement {
    Exited(ExitStatus),
    Failed(SecretChildError),
}

/// Run one secret-bearing process while retaining and cleaning its whole tree.
pub async fn run_secret_child(
    request: SecretChildRequest,
) -> Result<SecretChildOutput, SecretChildError> {
    run_child(request, None).await
}

/// A bounded exchange with no inherited application environment.
/// Cancellation of the caller still leaves an owner to reap the child tree.
pub async fn run_protocol_child(
    request: SecretChildRequest,
    max_stdout: usize,
) -> Result<SecretChildOutput, SecretChildError> {
    validate_secret_request(&request)?;
    let cancel = request.cancel.clone();
    let owned_cancel = CancelToken::new();
    let request = SecretChildRequest {
        cancel: owned_cancel.clone(),
        ..request
    };
    let (sender, mut receiver) = tokio::sync::oneshot::channel();
    // The private runtime can complete process cleanup after the caller's
    // runtime stops. The caller waits only for a bounded cleanup interval.
    let external_cancel = cancel.clone();
    let worker_cancel = owned_cancel.clone();
    let thread = std::thread::Builder::new()
        .name("ego-protocol-filter".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(SecretChildError::Launch)
                .and_then(|runtime| {
                    runtime.block_on(async {
                        if external_cancel.is_cancelled() {
                            worker_cancel.cancel(ego_core::event::domain::CancelCause::Host);
                        }
                        let mut task = tokio::spawn(run_child(request, Some(max_stdout)));
                        tokio::select! {
                            result = &mut task => result,
                            _ = external_cancel.cancelled() => {
                                worker_cancel.cancel(ego_core::event::domain::CancelCause::Host);
                                task.await
                            }
                        }
                        .map_err(|_| {
                            SecretChildError::InvalidRequest("protocol_filter child owner failed")
                        })?
                    })
                });
            let _ = sender.send(result);
        })
        .map_err(SecretChildError::Launch)?;
    struct ProtocolThread {
        cancel: CancelToken,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl ProtocolThread {
        fn finish(&mut self) -> bool {
            self.cancel
                .cancel(ego_core::event::domain::CancelCause::Host);
            // At the product maximum, command execution is 1s; group and
            // pipe cleanup add at most four 100ms graces under a responsive
            // OS. Keep additional startup room, but never block the caller
            // indefinitely if OS spawn or wait cannot complete.
            let deadline = Instant::now() + Duration::from_secs(2);
            while let Some(thread) = self.thread.as_ref() {
                if thread.is_finished() {
                    return self.thread.take().expect("finished thread").join().is_ok();
                }
                if Instant::now() >= deadline {
                    self.thread.take(); // Owner continues cleanup off-runtime.
                    return false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            true
        }
    }
    impl Drop for ProtocolThread {
        fn drop(&mut self) {
            if !self.finish() {
                tracing::warn!("protocol_filter process-tree cleanup is uncertain");
            }
        }
    }
    let mut owner = ProtocolThread {
        cancel: owned_cancel,
        thread: Some(thread),
    };
    let result = tokio::select! {
        result = &mut receiver => result.ok(),
        _ = cancel.cancelled() => None,
    };
    let complete = owner.finish();
    if !complete {
        return Err(SecretChildError::CleanupUncertain {
            cause: Some(Box::new(SecretChildError::Cancelled)),
        });
    }
    let result = result.or_else(|| receiver.try_recv().ok());
    match result {
        Some(Err(error @ SecretChildError::CleanupUncertain { .. })) => Err(error),
        _ if cancel.is_cancelled() => Err(SecretChildError::Cancelled),
        Some(result) => result,
        None => Err(SecretChildError::InvalidRequest(
            "protocol_filter child owner failed",
        )),
    }
}

async fn run_child(
    request: SecretChildRequest,
    max_stdout: Option<usize>,
) -> Result<SecretChildOutput, SecretChildError> {
    let program = validate_secret_request(&request)?;
    let pipe_grace = max_stdout.map(|_| request.termination_grace);

    let mut command = tokio::process::Command::new(program);
    command
        .args(&request.argv[1..])
        .stdin(if request.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if max_stdout.is_some() {
        command.env_clear();
        #[cfg(unix)]
        command.env("PATH", "/usr/bin:/bin");
        #[cfg(windows)]
        for name in ["SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
    }
    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd.as_path());
    }
    let mut child = ManagedChild::spawn(command).map_err(SecretChildError::Launch)?;
    let Some(stdout) = child.take_stdout() else {
        child.terminate(request.termination_grace).await;
        return Err(SecretChildError::InvalidRequest(
            "credential command stdout pipe was not installed",
        ));
    };
    let mut stdout_task = Some(tokio::spawn(read_secret(stdout, max_stdout)));
    let mut stdin_task = match (request.stdin, child.take_stdin()) {
        (Some(secret), Some(stdin)) => Some(tokio::spawn(write_secret(stdin, secret))),
        (Some(_), None) => {
            child.terminate(request.termination_grace).await;
            let _ = finish_stdout(stdout_task.take(), pipe_grace).await;
            return Err(SecretChildError::InvalidRequest(
                "credential command stdin pipe was not installed",
            ));
        }
        (None, _) => None,
    };

    let mut stdout_result = None;
    let settlement = wait_secret_child(
        &mut child,
        &request.cancel,
        request.run_deadline,
        request.command_timeout,
        &mut stdin_task,
        &mut stdout_task,
        &mut stdout_result,
    )
    .await;

    let (status, cleanup_complete) = match settlement {
        Settlement::Exited(status) => (
            Some(status),
            child.cleanup_after_exit(request.termination_grace).await,
        ),
        Settlement::Failed(error) => {
            let cleanup = child.terminate(request.termination_grace).await;
            let _ = finish_stdin(stdin_task.take(), pipe_grace).await;
            let _ = finish_stdout(stdout_task.take(), pipe_grace).await;
            return if cleanup {
                Err(error)
            } else {
                Err(SecretChildError::CleanupUncertain {
                    cause: Some(Box::new(error)),
                })
            };
        }
    };

    let stdin_result = finish_stdin(stdin_task.take(), pipe_grace).await;
    let pending_stdout = if pipe_grace.is_none() && stdin_result.is_err() {
        // The legacy credential path used to return on this input error.
        // Do not introduce an unbounded wait for an inherited stdout pipe.
        if let Some(task) = stdout_task.take() {
            task.abort();
        }
        None
    } else if stdout_result.is_none() {
        Some(finish_stdout(stdout_task.take(), pipe_grace).await)
    } else {
        None
    };
    if !cleanup_complete {
        return Err(SecretChildError::CleanupUncertain {
            cause: match status.as_ref() {
                Some(status) if status.success() => None,
                Some(status) => Some(Box::new(SecretChildError::Exit(status.code()))),
                None => None,
            },
        });
    }
    stdin_result?;
    if let Some(output) = pending_stdout {
        stdout_result = Some(output?);
    }
    let status = status.expect("an exited settlement has a status");
    if !status.success() {
        return Err(SecretChildError::Exit(status.code()));
    }
    Ok(SecretChildOutput {
        stdout: stdout_result.expect("stdout reaches EOF after child exit"),
    })
}

fn validate_secret_request(request: &SecretChildRequest) -> Result<&OsString, SecretChildError> {
    if request.cancel.is_cancelled() {
        return Err(SecretChildError::Cancelled);
    }
    if request
        .run_deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        return Err(SecretChildError::RunTimeout);
    }
    if request.command_timeout.is_zero() {
        return Err(SecretChildError::InvalidRequest(
            "credential command timeout must be greater than zero",
        ));
    }
    if request.termination_grace.is_zero() {
        return Err(SecretChildError::InvalidRequest(
            "credential command termination grace must be greater than zero",
        ));
    }
    let Some(program) = request.argv.first() else {
        return Err(SecretChildError::InvalidRequest(
            "credential command argv must not be empty",
        ));
    };

    Ok(program)
}

async fn wait_secret_child(
    child: &mut ManagedChild,
    cancel: &CancelToken,
    run_deadline: Option<Instant>,
    command_timeout: Duration,
    stdin_task: &mut Option<JoinHandle<std::io::Result<()>>>,
    stdout_task: &mut Option<JoinHandle<std::io::Result<SecretMaterial>>>,
    stdout_result: &mut Option<SecretMaterial>,
) -> Settlement {
    let started = Instant::now();
    loop {
        if cancel.is_cancelled() {
            break Settlement::Failed(SecretChildError::Cancelled);
        }
        if run_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break Settlement::Failed(SecretChildError::RunTimeout);
        }
        if started.elapsed() >= command_timeout {
            break Settlement::Failed(SecretChildError::CommandTimeout);
        }
        match child.try_wait() {
            Ok(Some(status)) => break Settlement::Exited(status),
            Ok(None) => {}
            Err(error) => break Settlement::Failed(SecretChildError::Io(error)),
        }
        if stdin_task.as_ref().is_some_and(JoinHandle::is_finished) {
            let task = stdin_task.take().expect("finished stdin task is present");
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => break Settlement::Failed(SecretChildError::Io(error)),
                Err(error) => {
                    break Settlement::Failed(SecretChildError::Io(std::io::Error::other(error)));
                }
            }
        }
        if stdout_task.as_ref().is_some_and(JoinHandle::is_finished) {
            let task = stdout_task.take().expect("finished stdout task is present");
            match task.await {
                Ok(Ok(stdout)) => *stdout_result = Some(stdout),
                Ok(Err(error)) => break Settlement::Failed(SecretChildError::Io(error)),
                Err(error) => {
                    break Settlement::Failed(SecretChildError::Io(std::io::Error::other(error)));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn write_secret(
    mut stdin: tokio::process::ChildStdin,
    secret: SecretMaterial,
) -> std::io::Result<()> {
    stdin.write_all(secret.expose_secret_bytes()).await?;
    stdin.shutdown().await
}

async fn read_secret(
    stdout: tokio::process::ChildStdout,
    limit: Option<usize>,
) -> std::io::Result<SecretMaterial> {
    let mut bytes = Zeroizing::new(Vec::new());
    let mut stdout = stdout.take(limit.map_or(u64::MAX, |limit| limit as u64 + 1));
    if let Err(error) = stdout.read_to_end(&mut bytes).await {
        bytes.zeroize();
        return Err(error);
    }
    if limit.is_some_and(|limit| bytes.len() > limit) {
        return Err(std::io::Error::other(
            "protocol_filter stdout exceeds limits",
        ));
    }
    Ok(SecretMaterial::new(std::mem::take(&mut *bytes)))
}

async fn finish_stdin(
    task: Option<JoinHandle<std::io::Result<()>>>,
    grace: Option<Duration>,
) -> Result<(), SecretChildError> {
    let Some(task) = task else {
        return Ok(());
    };
    join_pipe(task, grace).await
}

async fn finish_stdout(
    task: Option<JoinHandle<std::io::Result<SecretMaterial>>>,
    grace: Option<Duration>,
) -> Result<SecretMaterial, SecretChildError> {
    let Some(task) = task else {
        return Err(SecretChildError::InvalidRequest(
            "credential command stdout task was absent",
        ));
    };
    join_pipe(task, grace).await
}

async fn join_pipe<T>(
    mut task: JoinHandle<std::io::Result<T>>,
    grace: Option<Duration>,
) -> Result<T, SecretChildError> {
    let result = if let Some(grace) = grace {
        match tokio::time::timeout(grace, &mut task).await {
            Ok(result) => result,
            Err(_) => {
                task.abort();
                let _ = task.await;
                return Err(SecretChildError::InvalidRequest(
                    "protocol_filter pipe cleanup timed out",
                ));
            }
        }
    } else {
        task.await
    };
    result
        .map_err(|error| SecretChildError::Io(std::io::Error::other(error)))?
        .map_err(SecretChildError::Io)
}
