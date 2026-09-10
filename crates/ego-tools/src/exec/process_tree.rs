//! Shared operating-system process-tree ownership and cleanup.

use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

#[cfg(windows)]
type PlatformChild = Box<dyn process_wrap::tokio::ChildWrapper>;

#[cfg(not(windows))]
type PlatformChild = tokio::process::Child;

/// One child whose complete operating-system process tree remains owned here.
pub(super) struct ManagedChild {
    inner: PlatformChild,
    tree_id: Option<u32>,
}

impl ManagedChild {
    pub(super) fn spawn(command: Command) -> std::io::Result<Self> {
        #[cfg(windows)]
        let inner = {
            use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject, KillOnDrop};
            use windows::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

            let mut command = CommandWrap::from(command);
            command.wrap(CreationFlags(CREATE_NEW_PROCESS_GROUP));
            command.wrap(KillOnDrop);
            command.wrap(JobObject);
            command.spawn()?
        };

        #[cfg(not(windows))]
        let inner = {
            let mut command = command;
            command.kill_on_drop(true);
            configure_process_group(&mut command);
            command.spawn()?
        };

        let tree_id = inner.id();
        Ok(Self { inner, tree_id })
    }

    pub(super) fn tree_id(&self) -> Option<u32> {
        self.tree_id
    }

    #[cfg(windows)]
    pub(super) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.inner.stdin().take()
    }

    #[cfg(not(windows))]
    pub(super) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.inner.stdin.take()
    }

    #[cfg(windows)]
    pub(super) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.inner.stdout().take()
    }

    #[cfg(not(windows))]
    pub(super) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.inner.stdout.take()
    }

    #[cfg(windows)]
    pub(super) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.inner.stderr().take()
    }

    #[cfg(not(windows))]
    pub(super) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.inner.stderr.take()
    }

    pub(super) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.inner.try_wait()
    }

    pub(super) async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.inner.wait().await
    }

    #[cfg(windows)]
    pub(super) async fn terminate(&mut self, grace: Duration) -> bool {
        match self.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => return false,
        }
        if self.inner.start_kill().is_err() {
            return false;
        }
        tokio::time::timeout(grace, self.wait())
            .await
            .is_ok_and(|result| result.is_ok())
    }

    #[cfg(not(windows))]
    pub(super) async fn terminate(&mut self, grace: Duration) -> bool {
        terminate_group(self.tree_id, grace).await;
        if self.try_wait().ok().flatten().is_none() && self.inner.kill().await.is_err() {
            return false;
        }
        self.wait().await.is_ok() && wait_for_tree_absence(self.tree_id, grace).await
    }

    #[cfg(windows)]
    pub(super) async fn cleanup_after_exit(&mut self, _grace: Duration) -> bool {
        self.wait().await.is_ok()
    }

    #[cfg(not(windows))]
    pub(super) async fn cleanup_after_exit(&mut self, grace: Duration) -> bool {
        terminate_group(self.tree_id, grace).await;
        wait_for_tree_absence(self.tree_id, grace).await
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(any(unix, windows)))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(not(windows))]
async fn terminate_group(tree_id: Option<u32>, grace: Duration) {
    if signal_tree(tree_id, false).await {
        tokio::time::sleep(grace).await;
        let _ = signal_tree(tree_id, true).await;
    }
}

#[cfg(not(windows))]
async fn wait_for_tree_absence(tree_id: Option<u32>, grace: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        if tree_is_absent(tree_id).await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(unix)]
async fn tree_is_absent(tree_id: Option<u32>) -> bool {
    use nix::sys::signal::killpg;
    use nix::unistd::Pid;

    let Some(id) = tree_id else {
        return false;
    };
    let Ok(id) = i32::try_from(id) else {
        return false;
    };
    matches!(
        killpg(Pid::from_raw(id), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

#[cfg(not(any(unix, windows)))]
async fn tree_is_absent(_tree_id: Option<u32>) -> bool {
    false
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

#[cfg(not(any(unix, windows)))]
async fn signal_tree(_tree_id: Option<u32>, _force: bool) -> bool {
    false
}
