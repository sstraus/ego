//! Shared operating-system process-group ownership and cleanup.

use std::time::Duration;

use tokio::process::Child;

#[cfg(unix)]
pub(super) fn configure_process_group(command: &mut tokio::process::Command) {
    command.process_group(0);
}

#[cfg(windows)]
pub(super) fn configure_process_group(command: &mut tokio::process::Command) {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
pub(super) fn configure_process_group(_command: &mut tokio::process::Command) {}

pub(super) async fn terminate_tree(
    child: &mut Child,
    tree_id: Option<u32>,
    grace: Duration,
) -> bool {
    terminate_group(tree_id, grace).await;
    if child.try_wait().ok().flatten().is_none() && child.kill().await.is_err() {
        return false;
    }
    child.wait().await.is_ok() && wait_for_tree_absence(tree_id, grace).await
}

pub(super) async fn cleanup_descendants(tree_id: Option<u32>, grace: Duration) -> bool {
    terminate_group(tree_id, grace).await;
    wait_for_tree_absence(tree_id, grace).await
}

pub(super) async fn terminate_group(tree_id: Option<u32>, grace: Duration) {
    if signal_tree(tree_id, false).await {
        tokio::time::sleep(grace).await;
        let _ = signal_tree(tree_id, true).await;
    }
}

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

#[cfg(windows)]
async fn tree_is_absent(tree_id: Option<u32>) -> bool {
    let Some(id) = tree_id else {
        return false;
    };
    let filter = format!("PID eq {id}");
    let output = tokio::process::Command::new("tasklist")
        .args(["/FI", &filter, "/NH"])
        .output()
        .await;
    output.is_ok_and(|output| {
        output.status.success()
            && !String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .any(|field| field == id.to_string())
    })
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
