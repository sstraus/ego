//! Terminal-native lifecycle for the operator-selected config editor.

use std::ffi::OsString;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use ego_core::turn::CancelToken;

use super::process_tree::ManagedChild;

const TERMINATION_GRACE: Duration = Duration::from_millis(500);

/// A config editor process failed before its candidate could be published.
#[derive(Debug)]
pub enum EditorProcessError {
    InvalidRequest(&'static str),
    Launch(std::io::Error),
    Wait(std::io::Error),
    Terminal(std::io::Error),
    Cancelled,
    Exit(Option<i32>),
    CleanupUncertain {
        cause: Option<Box<EditorProcessError>>,
    },
}

impl std::fmt::Display for EditorProcessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(reason) => formatter.write_str(reason),
            Self::Launch(error) => write!(formatter, "could not launch config editor: {error}"),
            Self::Wait(error) => write!(formatter, "could not wait for config editor: {error}"),
            Self::Terminal(error) => {
                write!(
                    formatter,
                    "could not transfer the terminal for the config editor: {error}"
                )
            }
            Self::Cancelled => formatter.write_str("config editor was cancelled"),
            Self::Exit(Some(code)) => write!(formatter, "config editor exited with code {code}"),
            Self::Exit(None) => formatter.write_str("config editor exited by signal"),
            Self::CleanupUncertain { cause } => {
                formatter.write_str("config editor process-tree cleanup is uncertain")?;
                if let Some(cause) = cause {
                    write!(formatter, " after {cause}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for EditorProcessError {}

enum Settlement {
    Exited(ExitStatus),
    Failed(EditorProcessError),
}

/// Run one editor argv with inherited terminal streams and owned tree cleanup.
pub async fn run_editor_process(
    argv: Vec<OsString>,
    cancel: CancelToken,
) -> Result<(), EditorProcessError> {
    let Some(program) = argv.first() else {
        return Err(EditorProcessError::InvalidRequest(
            "config editor argv must not be empty",
        ));
    };
    if program.is_empty() {
        return Err(EditorProcessError::InvalidRequest(
            "config editor program must not be empty",
        ));
    }
    if cancel.is_cancelled() {
        return Err(EditorProcessError::Cancelled);
    }

    let mut command = tokio::process::Command::new(program);
    command
        .args(&argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut child = ManagedChild::spawn(command).map_err(EditorProcessError::Launch)?;
    let tree_id = child.tree_id();
    let mut terminal = match ForegroundTerminal::acquire(tree_id) {
        Ok(terminal) => terminal,
        Err(error) => {
            let cleanup = child.terminate(TERMINATION_GRACE).await;
            return if cleanup {
                Err(EditorProcessError::Terminal(error))
            } else {
                Err(EditorProcessError::CleanupUncertain {
                    cause: Some(Box::new(EditorProcessError::Terminal(error))),
                })
            };
        }
    };

    let settlement = loop {
        if cancel.is_cancelled() {
            break Settlement::Failed(EditorProcessError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break Settlement::Exited(status),
            Ok(None) => tokio::time::sleep(Duration::from_millis(5)).await,
            Err(error) => break Settlement::Failed(EditorProcessError::Wait(error)),
        }
    };

    let (status, error, cleanup) = match settlement {
        Settlement::Exited(status) => {
            let cleanup = child.cleanup_after_exit(TERMINATION_GRACE).await;
            (Some(status), None, cleanup)
        }
        Settlement::Failed(error) => {
            let cleanup = child.terminate(TERMINATION_GRACE).await;
            (None, Some(error), cleanup)
        }
    };
    let terminal_result = terminal.restore();
    if !cleanup {
        return Err(EditorProcessError::CleanupUncertain {
            cause: error.map(Box::new),
        });
    }
    if let Err(error) = terminal_result {
        return Err(EditorProcessError::Terminal(error));
    }
    if let Some(error) = error {
        return Err(error);
    }
    let status = status.expect("an exited editor settlement has a status");
    if !status.success() {
        return Err(EditorProcessError::Exit(status.code()));
    }
    Ok(())
}

#[cfg(unix)]
struct ForegroundTerminal {
    previous_group: Option<nix::unistd::Pid>,
}

#[cfg(unix)]
impl ForegroundTerminal {
    fn acquire(tree_id: Option<u32>) -> std::io::Result<Self> {
        use std::io::IsTerminal as _;

        if !std::io::stdin().is_terminal() {
            return Ok(Self {
                previous_group: None,
            });
        }
        let tree_id = tree_id
            .and_then(|id| i32::try_from(id).ok())
            .map(nix::unistd::Pid::from_raw)
            .ok_or_else(|| std::io::Error::other("config editor process has no process group"))?;
        let previous_group =
            nix::unistd::tcgetpgrp(std::io::stdin()).map_err(std::io::Error::from)?;
        set_foreground_group(tree_id)?;
        Ok(Self {
            previous_group: Some(previous_group),
        })
    }

    fn restore(&mut self) -> std::io::Result<()> {
        let Some(previous_group) = self.previous_group else {
            return Ok(());
        };
        set_foreground_group(previous_group)?;
        self.previous_group = None;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for ForegroundTerminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(unix)]
fn set_foreground_group(group: nix::unistd::Pid) -> std::io::Result<()> {
    use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};

    let mut blocked = SigSet::empty();
    blocked.add(Signal::SIGTTOU);
    let mut previous = SigSet::empty();
    pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&blocked), Some(&mut previous))
        .map_err(std::io::Error::from)?;
    let changed = nix::unistd::tcsetpgrp(std::io::stdin(), group).map_err(std::io::Error::from);
    let restore = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&previous), None)
        .map_err(std::io::Error::from);
    match (changed, restore) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[cfg(not(unix))]
struct ForegroundTerminal;

#[cfg(not(unix))]
impl ForegroundTerminal {
    fn acquire(_tree_id: Option<u32>) -> std::io::Result<Self> {
        Ok(Self)
    }

    fn restore(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
