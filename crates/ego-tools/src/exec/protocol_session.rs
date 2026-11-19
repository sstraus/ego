//! Bounded line exchanges with an independently owned process tree.

use std::process::Stdio;
use std::time::Instant;

use ego_core::credentials::SecretMaterial;
use ego_core::event::domain::CancelCause;
use ego_core::turn::CancelToken;
use futures::future::BoxFuture;
use serde::Deserialize as _;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use super::process_tree::ManagedChild;
use super::secret_child::{SecretChildError, SecretChildRequest};

type Reply = oneshot::Sender<Option<SecretMaterial>>;

enum Command {
    Exchange(SecretMaterial, Reply),
    Finish(Reply),
}

/// One caller at a time; dropping this handle or an exchange cancels the owner.
pub struct ProtocolChild {
    commands: mpsc::Sender<Command>,
    terminal: oneshot::Receiver<Result<(), SecretChildError>>,
    cancel: CancelToken,
    finished: bool,
}

impl ProtocolChild {
    pub fn exchange(
        &mut self,
        input: SecretMaterial,
    ) -> BoxFuture<'_, Result<SecretMaterial, SecretChildError>> {
        let guard = CancelOnDrop(Some(self.cancel.clone()));
        Box::pin(async move {
            let mut guard = guard;
            let result = self.request(Some(input)).await.and_then(|output| {
                output.ok_or(SecretChildError::InvalidRequest(
                    "protocol_filter exchange returned no frame",
                ))
            });
            guard.0.take();
            result
        })
    }

    pub fn finish(&mut self) -> BoxFuture<'_, Result<(), SecretChildError>> {
        let guard = CancelOnDrop(Some(self.cancel.clone()));
        Box::pin(async move {
            let mut guard = guard;
            let result = self.request(None).await.map(|_| ());
            guard.0.take();
            result
        })
    }

    async fn request(
        &mut self,
        input: Option<SecretMaterial>,
    ) -> Result<Option<SecretMaterial>, SecretChildError> {
        if self.finished {
            return Err(SecretChildError::InvalidRequest(
                "protocol_filter session is closed",
            ));
        }
        let finishing = input.is_none();
        let (reply, receiver) = oneshot::channel();
        let command = match input {
            Some(input) => Command::Exchange(input, reply),
            None => Command::Finish(reply),
        };
        if self.commands.send(command).await.is_ok()
            && let Ok(output) = receiver.await
        {
            self.finished = finishing;
            return Ok(output);
        }
        self.finished = true;
        match (&mut self.terminal).await {
            Ok(Err(error)) => Err(error),
            _ => Err(SecretChildError::CleanupUncertain { cause: None }),
        }
    }
}

impl Drop for ProtocolChild {
    fn drop(&mut self) {
        self.cancel.cancel(CancelCause::Host);
    }
}

struct CancelOnDrop(Option<CancelToken>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            cancel.cancel(CancelCause::Host);
        }
    }
}

/// Start an exact argv command on a private runtime. No ambient runtime is used.
/// `max_frame` excludes the line terminator, for both input and output.
pub fn start_protocol_session(
    request: SecretChildRequest,
    max_frame: usize,
) -> Result<ProtocolChild, SecretChildError> {
    validate_request(&request, max_frame)?;
    let startup_timeout = request.run_deadline.map_or(request.command_timeout, |end| {
        request
            .command_timeout
            .min(end.saturating_duration_since(Instant::now()))
    });
    let cancel = CancelToken::new();
    let owner_cancel = cancel.clone();
    let (commands, receiver) = mpsc::channel(1);
    let (terminal_sender, terminal) = oneshot::channel();
    let (started, startup) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("ego-protocol-session".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = started.send(Err(SecretChildError::Launch(error)));
                    return;
                }
            };
            runtime.block_on(async move {
                let mut command = tokio::process::Command::new(&request.argv[0]);
                command
                    .args(&request.argv[1..])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .env_clear();
                #[cfg(unix)]
                command.env("PATH", "/usr/bin:/bin");
                #[cfg(windows)]
                for name in ["SystemRoot", "WINDIR"] {
                    if let Some(value) = std::env::var_os(name) {
                        command.env(name, value);
                    }
                }
                if let Some(cwd) = &request.cwd {
                    command.current_dir(cwd.as_path());
                }
                let mut child = match ManagedChild::spawn(command) {
                    Ok(child) => child,
                    Err(error) => {
                        let _ = started.send(Err(SecretChildError::Launch(error)));
                        return;
                    }
                };
                // Startup acknowledgement never transfers ownership of the child.
                if started.send(Ok(())).is_err() {
                    let _ = child.terminate(request.termination_grace).await;
                    return;
                }
                let (result, pending) =
                    own_session(&mut child, &request, owner_cancel, receiver, max_frame).await;
                let clean = match child.try_wait() {
                    Ok(Some(_)) => child.cleanup_after_exit(request.termination_grace).await,
                    _ => child.terminate(request.termination_grace).await,
                };
                let result = if clean {
                    result
                } else {
                    Err(SecretChildError::CleanupUncertain {
                        cause: result.err().map(Box::new),
                    })
                };
                match (result, pending) {
                    (Ok(()), Some(reply)) => {
                        let _ = terminal_sender.send(Ok(()));
                        let _ = reply.send(None);
                    }
                    (result, pending) => {
                        // Publish cleanup outcome before closing the active reply.
                        let _ = terminal_sender.send(result);
                        drop(pending);
                    }
                }
            });
        })
        .map_err(SecretChildError::Launch)?;
    match startup.recv_timeout(startup_timeout) {
        Ok(result) => result?,
        Err(_) => {
            cancel.cancel(CancelCause::Host);
            // The owner retains any process still being spawned and cleans it
            // off-runtime. Without its acknowledgement cleanup is unproven.
            return Err(SecretChildError::CleanupUncertain {
                cause: Some(Box::new(SecretChildError::CommandTimeout)),
            });
        }
    }
    Ok(ProtocolChild {
        commands,
        terminal,
        cancel,
        finished: false,
    })
}

// Same launch policy as run_protocol_child: native argv, no shell expansion.
fn validate_request(
    request: &SecretChildRequest,
    max_frame: usize,
) -> Result<(), SecretChildError> {
    if request.cancel.is_cancelled() {
        return Err(SecretChildError::Cancelled);
    }
    if request
        .run_deadline
        .is_some_and(|end| Instant::now() >= end)
    {
        return Err(SecretChildError::RunTimeout);
    }
    if request.argv.is_empty()
        || request.command_timeout.is_zero()
        || request.termination_grace.is_zero()
        || request.stdin.is_some()
        || max_frame == 0
    {
        return Err(SecretChildError::InvalidRequest(
            "protocol_filter requires argv, positive bounds and no initial stdin",
        ));
    }
    Ok(())
}

async fn stopped(request: &SecretChildRequest, cancel: &CancelToken) -> SecretChildError {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => SecretChildError::Cancelled,
        _ = request.cancel.cancelled() => SecretChildError::Cancelled,
        _ = async {
            if let Some(end) = request.run_deadline {
                tokio::time::sleep_until(end.into()).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => SecretChildError::RunTimeout,
    }
}

async fn own_session(
    child: &mut ManagedChild,
    request: &SecretChildRequest,
    cancel: CancelToken,
    mut commands: mpsc::Receiver<Command>,
    max_frame: usize,
) -> (Result<(), SecretChildError>, Option<Reply>) {
    let (Some(stdin), Some(stdout)) = (child.take_stdin(), child.take_stdout()) else {
        return (
            Err(SecretChildError::InvalidRequest(
                "protocol_filter pipes absent",
            )),
            None,
        );
    };
    let mut stdin = Some(stdin);
    let mut stdout = BufReader::with_capacity(8192, stdout);
    loop {
        let mut probe = Zeroizing::new([0_u8; 1]);
        let command = tokio::select! {
            biased;
            error = stopped(request, &cancel) => return (Err(error), None),
            read = stdout.read(&mut probe[..]) => {
                let error = match read {
                    Err(error) => SecretChildError::Io(error),
                    _ => SecretChildError::InvalidRequest("protocol_filter unsolicited stdout or early EOF"),
                };
                return (Err(error), None);
            }
            status = child.wait() => {
                let error = match status {
                    Ok(status) if !status.success() => SecretChildError::Exit(status.code()),
                    Ok(_) => SecretChildError::InvalidRequest("protocol_filter exited before finish"),
                    Err(error) => SecretChildError::Io(error),
                };
                return (Err(error), None);
            }
            command = commands.recv() => command,
        };
        let Some(command) = command else {
            return (Err(SecretChildError::Cancelled), None);
        };
        let (input, mut reply) = match command {
            Command::Exchange(input, reply) => (Some(input), reply),
            Command::Finish(reply) => (None, reply),
        };
        let finishing = input.is_none();
        let operation = async {
            if let Some(input) = input {
                validate_frame(input.expose_secret_bytes(), max_frame)?;
                let stdin = stdin.as_mut().expect("stdin remains open until finish");
                stdin
                    .write_all(input.expose_secret_bytes())
                    .await
                    .map_err(SecretChildError::Io)?;
                stdin.write_all(b"\n").await.map_err(SecretChildError::Io)?;
                stdin.flush().await.map_err(SecretChildError::Io)?;
                read_frame(&mut stdout, max_frame).await.map(Some)
            } else {
                let mut stdin = stdin.take().expect("finish owns stdin");
                stdin.shutdown().await.map_err(SecretChildError::Io)?;
                // Closing the handle is needed on platforms where shutdown is a no-op.
                drop(stdin);
                let status = child.wait().await.map_err(SecretChildError::Io)?;
                if !child.cleanup_after_exit(request.termination_grace).await {
                    return Err(SecretChildError::CleanupUncertain { cause: None });
                }
                let mut extra = Zeroizing::new([0_u8; 1]);
                if stdout
                    .read(&mut extra[..])
                    .await
                    .map_err(SecretChildError::Io)?
                    != 0
                {
                    return Err(SecretChildError::InvalidRequest(
                        "protocol_filter additional stdout",
                    ));
                }
                if !status.success() {
                    return Err(SecretChildError::Exit(status.code()));
                }
                Ok(None)
            }
        };
        let result = tokio::select! {
            biased;
            error = stopped(request, &cancel) => Err(error),
            _ = reply.closed() => Err(SecretChildError::Cancelled),
            result = tokio::time::timeout(request.command_timeout, operation) => {
                result.unwrap_or(Err(SecretChildError::CommandTimeout))
            }
        };
        match result {
            Ok(_) if finishing => return (Ok(()), Some(reply)),
            Ok(output) => {
                if reply.send(output).is_err() {
                    return (Err(SecretChildError::Cancelled), None);
                }
            }
            Err(error) => return (Err(error), Some(reply)),
        }
    }
}

fn validate_frame(bytes: &[u8], max_frame: usize) -> Result<(), SecretChildError> {
    if bytes.len() > max_frame || bytes.contains(&b'\n') {
        return Err(SecretChildError::InvalidRequest(
            "protocol_filter frame exceeds limits or contains newline",
        ));
    }
    // IgnoredAny skips string contents, rather than building an unredacted JSON tree.
    let mut json = serde_json::Deserializer::from_slice(bytes);
    if serde::de::IgnoredAny::deserialize(&mut json).is_err() || json.end().is_err() {
        return Err(SecretChildError::InvalidRequest(
            "protocol_filter frame is not JSON",
        ));
    }
    Ok(())
}

async fn read_frame(
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    max_frame: usize,
) -> Result<SecretMaterial, SecretChildError> {
    let mut frame = Zeroizing::new(Vec::new());
    loop {
        let available = stdout.fill_buf().await.map_err(SecretChildError::Io)?;
        if available.is_empty() {
            return Err(SecretChildError::InvalidRequest(
                "protocol_filter truncated stdout frame",
            ));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(available.len());
        if frame.len().saturating_add(count) > max_frame {
            return Err(SecretChildError::InvalidRequest(
                "protocol_filter stdout frame exceeds limits",
            ));
        }
        frame.extend_from_slice(&available[..count]);
        stdout.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            validate_frame(&frame, max_frame)?;
            return Ok(SecretMaterial::new(std::mem::take(&mut *frame)));
        }
    }
}
