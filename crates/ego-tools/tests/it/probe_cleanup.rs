#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::future::{Future as _, poll_fn};
use std::task::Poll;
use std::time::Duration;

use ego_core::event::domain::CancelCause;
use ego_core::permit::{CanonPath, Confinement, ProcessError, RunConfinement};
use ego_core::turn::CancelToken;
use ego_tools::exec::{ExecLimits, RunEffectPorts};

// Catches: a cancelled capability probe leaves its workspace fixture behind.
#[tokio::test]
async fn cancelled_probe_removes_workspace_directory() {
    let workspace = tempfile::tempdir().expect("workspace");
    let policy = RunConfinement::new(
        Confinement::Workspace,
        false,
        CanonPath::new(std::fs::canonicalize(workspace.path()).expect("canonical workspace")),
    )
    .expect("workspace policy");
    let cancel = CancelToken::new();
    let mut measurement = Box::pin(RunEffectPorts::diagnostic_capabilities(
        &policy,
        cancel.clone(),
        ExecLimits::default(),
    ));
    // Observe an actual fixture through the public entry point before cancelling.
    // This prevents an early prerequisite failure from masquerading as cleanup.
    tokio::time::timeout(
        Duration::from_secs(30),
        poll_fn(|context| {
            let result = measurement.as_mut().poll(context);
            let fixture_exists = std::fs::read_dir(workspace.path())
                .expect("workspace entries")
                .any(|entry| {
                    entry
                        .expect("workspace entry")
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".ego-exec-probe-")
                });
            if fixture_exists {
                return Poll::Ready(());
            }
            assert!(
                result.is_pending(),
                "probe ended before fixture creation: {result:?}"
            );
            Poll::Pending
        }),
    )
    .await
    .expect("probe creates workspace fixture");
    cancel.cancel(CancelCause::User);
    let result = measurement.await;
    assert!(matches!(result, Err(ProcessError::Cancelled)), "{result:?}");
    let remaining: Vec<_> = std::fs::read_dir(workspace.path())
        .expect("workspace entries after cancellation")
        .map(|entry| entry.expect("workspace entry").file_name())
        .collect();
    assert!(remaining.is_empty(), "probe leaked: {remaining:?}");
}

// Catches: SIGKILL skips TempDir drop and the next public probe retains the debris.
#[tokio::test]
async fn next_probe_removes_dead_owner_directory_but_preserves_live_owner() {
    for dead in [true, false] {
        let root = tempfile::tempdir().expect("test root");
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let ready = root.path().join("ready");
        let mut owner = ProbeOwner(
            std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--ignored",
                    "--exact",
                    "probe_cleanup::hold_public_probe_fixture",
                    "--nocapture",
                ])
                .env("EGO_CLEANUP_WORKSPACE", &workspace)
                .env("EGO_CLEANUP_READY", &ready)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("start probe owner"),
        );
        tokio::time::timeout(Duration::from_secs(30), async {
            while !ready.exists() {
                assert!(
                    owner.0.try_wait().expect("owner status").is_none(),
                    "owner exited before creating fixture"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owner creates real fixture");
        let fixture =
            std::path::PathBuf::from(std::fs::read_to_string(&ready).expect("fixture path"));
        assert!(fixture.starts_with(&workspace));
        assert!(fixture.is_dir());
        // Similar names, unknown records, copied markers and symlinks do not
        // authorize deleting user data. Marker bytes come from the real owner.
        let unmarked = workspace.join(".ego-exec-probe-user");
        let unknown = workspace.join(".ego-exec-probe-unknown");
        let copied = workspace.join(".ego-exec-probe-copy");
        let symlinked = workspace.join(".ego-exec-probe-symlink");
        for directory in [&unmarked, &unknown, &copied] {
            std::fs::create_dir(directory).expect("unowned directory");
            std::fs::write(directory.join("keep"), b"user data").expect("user file");
        }
        std::fs::write(unknown.join(".ego-probe-owner"), b"unknown version").unwrap();
        // Before the fix there is no marker; the dead-owner assertion below
        // must still reach the original leak rather than fail fixture setup.
        if let Ok(marker) = std::fs::read(fixture.join(".ego-probe-owner")) {
            std::fs::write(copied.join(".ego-probe-owner"), marker).unwrap();
        }
        std::os::unix::fs::symlink(&fixture, &symlinked).expect("fixture symlink");
        if dead {
            owner.0.kill().expect("SIGKILL owner");
            owner.0.wait().expect("reap owner");
        }
        let policy = RunConfinement::new(
            Confinement::Workspace,
            false,
            CanonPath::new(std::fs::canonicalize(&workspace).expect("canonical workspace")),
        )
        .expect("workspace policy");
        let cancel = CancelToken::new();
        cancel.cancel(CancelCause::User);
        let result =
            RunEffectPorts::diagnostic_capabilities(&policy, cancel, ExecLimits::default()).await;
        assert!(matches!(result, Err(ProcessError::Cancelled)), "{result:?}");
        assert_eq!(
            fixture.is_dir(),
            !dead,
            "next probe must remove only a dead owner's fixture (dead={dead}): {fixture:?}"
        );
        for directory in [&unmarked, &unknown, &copied] {
            assert_eq!(std::fs::read(directory.join("keep")).unwrap(), b"user data");
        }
        assert_eq!(std::fs::read_link(&symlinked).unwrap(), fixture);
    }
}

struct ProbeOwner(std::process::Child);

impl Drop for ProbeOwner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// Subprocess fixture: production creates the directory and its ownership record.
// The parent kills only this child, so no handwritten owner metadata is needed.
#[test]
#[ignore = "subprocess fixture for probe cleanup"]
fn hold_public_probe_fixture() {
    let workspace =
        std::path::PathBuf::from(std::env::var_os("EGO_CLEANUP_WORKSPACE").expect("workspace"));
    let ready =
        std::path::PathBuf::from(std::env::var_os("EGO_CLEANUP_READY").expect("ready path"));
    let policy = RunConfinement::new(
        Confinement::Workspace,
        false,
        CanonPath::new(std::fs::canonicalize(&workspace).expect("canonical workspace")),
    )
    .expect("workspace policy");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let mut measurement = Box::pin(RunEffectPorts::diagnostic_capabilities(
                &policy,
                CancelToken::new(),
                ExecLimits::default(),
            ));
            let fixture = poll_fn(|context| {
                let result = measurement.as_mut().poll(context);
                if let Some(fixture) = std::fs::read_dir(&workspace)
                    .expect("workspace entries")
                    .map(|entry| entry.expect("workspace entry").path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".ego-exec-probe-")
                    })
                {
                    return Poll::Ready(fixture);
                }
                assert!(
                    result.is_pending(),
                    "probe ended before fixture creation: {result:?}"
                );
                Poll::Pending
            })
            .await;
            std::fs::write(ready, fixture.to_str().expect("fixture UTF-8")).expect("publish ready");
            std::future::pending::<()>().await;
            drop(measurement);
        });
}
