#![cfg(unix)]

//! contract for the protocol-neutral secret-child adapter.
//! Credential commands are owned by `ego-cli`; process groups and bounded
//! cleanup belong to `ego-tools`. Secret-bearing subprocesses must not use
//! ordinary tool evidence or the tool executor's null-stdin contract.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ego_core::credentials::SecretMaterial;
use ego_core::event::domain::CancelCause;
use ego_core::permit::CanonPath;
use ego_core::turn::CancelToken;
use ego_tools::exec::{SecretChildError, SecretChildRequest, run_secret_child};

struct Fixture {
    _root: tempfile::TempDir,
    cancel: CancelToken,
    direct: PathBuf,
    descendant: PathBuf,
    run_deadline: Option<Instant>,
}

fn fixture(run_deadline_after: Option<Duration>) -> Fixture {
    let root = tempfile::tempdir().expect("fixture root");
    let cancel = CancelToken::new();
    let direct = root.path().join("direct.pid");
    let descendant = root.path().join("descendant.pid");
    Fixture {
        _root: root,
        cancel,
        direct,
        descendant,
        run_deadline: run_deadline_after.map(|duration| Instant::now() + duration),
    }
}

fn spec(
    fixture: &Fixture,
    body: &str,
    timeout: Option<Duration>,
    stdin: Option<SecretMaterial>,
) -> SecretChildRequest {
    let argv = vec![
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        OsString::from(body),
        OsString::from("secret-child-fixture"),
        fixture.direct.as_os_str().to_os_string(),
        fixture.descendant.as_os_str().to_os_string(),
    ];
    SecretChildRequest {
        argv,
        cwd: Some(CanonPath::new(
            std::env::current_dir().expect("current directory"),
        )),
        stdin,
        cancel: fixture.cancel.clone(),
        run_deadline: fixture.run_deadline,
        command_timeout: timeout.unwrap_or(Duration::from_secs(5)),
        termination_grace: Duration::from_millis(100),
    }
}

fn tree_body() -> &'static str {
    r#"
printf '%s\n' "$$" > "$1"
sleep 5 &
printf '%s\n' "$!" > "$2"
while :; do sleep 1; done
"#
}

/// How long to wait for something that must happen.
///
/// It bounds how quickly a genuine failure is reported and nothing else: both
/// helpers below return the moment the condition holds, so a shorter window
/// makes no passing test stricter — it only decides how often a full-workspace
/// run, where thousands of tests compete for the same cores, reports a shell
/// that was merely slow as a shell that never ran.
const OBSERVATION_WINDOW: Duration = Duration::from_secs(15);

fn wait_for(path: &Path) {
    let deadline = Instant::now() + OBSERVATION_WINDOW;
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "secret-child fixture did not write {path:?}");
}

fn recorded_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
}

fn assert_gone(path: &Path) {
    let pid = recorded_pid(path)
        .expect("positive process ID in marker")
        .to_string();
    let deadline = Instant::now() + OBSERVATION_WINDOW;
    while Instant::now() < deadline {
        let alive = Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .output()
            .expect("kill utility")
            .status
            .success();
        if !alive {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let state = Command::new("ps")
        .args(["-p", pid.trim(), "-o", "pid,ppid,pgid,state,command"])
        .output()
        .expect("process state");
    panic!(
        "secret child {} survived cleanup: {}",
        pid.trim(),
        String::from_utf8_lossy(&state.stdout)
    );
}

fn assert_not_running(path: &Path) {
    let pid = recorded_pid(path)
        .expect("positive process ID in marker")
        .to_string();
    let deadline = Instant::now() + OBSERVATION_WINDOW;
    while Instant::now() < deadline {
        let state = Command::new("ps")
            .args(["-p", pid.trim(), "-o", "state="])
            .output()
            .expect("process state");
        assert!(
            state.stderr.is_empty(),
            "process inspection failed: {}",
            String::from_utf8_lossy(&state.stderr)
        );
        if !state.status.success() {
            assert!(
                state.stdout.is_empty(),
                "unexpected process inspection output"
            );
            return;
        }
        if state.stdout.first() == Some(&b'Z') {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "secret child {} is still running after runtime shutdown",
        pid.trim()
    );
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
}

#[cfg(unix)]
#[test]
fn cancellation_before_spawn_starts_no_secret_child() {
    let fixture = fixture(None);
    fixture.cancel.cancel(CancelCause::User);
    let result = runtime().block_on(run_secret_child(spec(
        &fixture,
        "printf '%s' \"$$\" > \"$1\"",
        None,
        None,
    )));

    assert!(
        matches!(result, Err(SecretChildError::Cancelled)),
        "{result:?}"
    );
    assert!(!fixture.direct.exists(), "cancelled child was spawned");
    assert!(
        !fixture.descendant.exists(),
        "cancelled child left a descendant"
    );
}

#[cfg(unix)]
#[test]
fn cancellation_after_spawn_terminates_waits_and_proves_the_complete_tree_absent() {
    let fixture = fixture(None);
    let cancel = fixture.cancel.clone();
    let descendant = fixture.descendant.clone();
    let canceller = std::thread::spawn(move || {
        wait_for(&descendant);
        cancel.cancel(CancelCause::User);
    });
    let result = runtime().block_on(run_secret_child(spec(
        &fixture,
        tree_body(),
        None,
        Some(SecretMaterial::new(b"refresh-request".to_vec())),
    )));
    canceller.join().expect("canceller");

    assert!(
        matches!(result, Err(SecretChildError::Cancelled)),
        "{result:?}"
    );
    assert_gone(&fixture.direct);
    assert_gone(&fixture.descendant);
}

#[cfg(unix)]
#[test]
fn independent_child_timeout_is_typed_and_cleans_the_complete_tree() {
    let fixture = fixture(None);
    // The tree has to exist before its cleanup means anything, and the script
    // writes its two markers before it starts looping. A timeout short enough
    // to fire before the shell has recorded itself leaves nothing to assert
    // about and reads as a missing marker file, so the observer proves the
    // tree is there and the timeout — which the body never ends on its own —
    // still fires well inside the window.
    let descendant = fixture.descendant.clone();
    let observer = std::thread::spawn(move || wait_for(&descendant));
    let result = runtime().block_on(run_secret_child(spec(
        &fixture,
        tree_body(),
        Some(Duration::from_secs(2)),
        None,
    )));
    observer.join().expect("process observer");

    assert!(
        matches!(result, Err(SecretChildError::CommandTimeout)),
        "{result:?}"
    );
    assert_gone(&fixture.direct);
    assert_gone(&fixture.descendant);
}

#[cfg(unix)]
#[test]
fn run_deadline_is_distinct_from_child_timeout_and_cleans_the_complete_tree() {
    let fixture = fixture(Some(Duration::from_secs(1)));
    let descendant = fixture.descendant.clone();
    let observer = std::thread::spawn(move || wait_for(&descendant));
    let result = runtime().block_on(run_secret_child(spec(&fixture, tree_body(), None, None)));
    observer.join().expect("process observer");

    assert!(
        matches!(result, Err(SecretChildError::RunTimeout)),
        "{result:?}"
    );
    assert_gone(&fixture.direct);
    assert_gone(&fixture.descendant);
}

#[test]
fn stdin_is_secret_material_and_successful_stdout_is_opaque_owned_material() {
    let fixture = fixture(None);
    let result = runtime().block_on(run_secret_child(spec(
        &fixture,
        "IFS= read -r request; printf '%s' \"$request\"",
        None,
        Some(SecretMaterial::new(b"stdin-secret\n".to_vec())),
    )));
    let output = result.expect("secret child succeeded");
    assert_eq!(output.stdout().expose_secret_bytes(), b"stdin-secret");
    assert!(!format!("{:?}", output.stdout()).contains("stdin-secret"));
}

#[cfg(unix)]
#[test]
fn partial_secret_stdout_is_discarded_on_failure_and_errors_are_redacted() {
    let fixture = fixture(None);
    let result = runtime().block_on(run_secret_child(spec(
        &fixture,
        "printf '%s' 'partial-secret'; printf '%s' 'stderr-secret' >&2; exit 17",
        None,
        None,
    )));
    let error = result.expect_err("failed secret child unexpectedly succeeded");
    let debug = format!("{error:?}");
    let display = error.to_string();
    assert!(
        !debug.contains("partial-secret"),
        "secret leaked in error debug"
    );
    assert!(
        !debug.contains("stderr-secret"),
        "stderr leaked in error debug"
    );
    assert!(
        !display.contains("partial-secret"),
        "secret leaked in error display"
    );
    assert!(
        !display.contains("stderr-secret"),
        "stderr leaked in error display"
    );
}

// Catches: cancellation of a filter leaving its child or descendants running.
#[test]
fn protocol_filter_cancellation_reaps_owned_tree() {
    let fixture = fixture(None);
    let cancel = fixture.cancel.clone();
    let descendant = fixture.descendant.clone();
    let canceller = std::thread::spawn(move || {
        wait_for(&descendant);
        cancel.cancel(CancelCause::User);
    });
    let result = runtime().block_on(ego_tools::exec::run_protocol_child(
        spec(
            &fixture,
            tree_body(),
            None,
            Some(SecretMaterial::new(vec![])),
        ),
        1024,
    ));
    canceller.join().unwrap();
    assert!(
        matches!(result, Err(SecretChildError::Cancelled)),
        "{result:?}"
    );
    assert_gone(&fixture.direct);
    assert_gone(&fixture.descendant);
}

// Catches: Tokio shutdown aborting filter cleanup before a TERM-ignoring descendant is killed.
#[test]
fn protocol_filter_runtime_shutdown_kills_term_ignoring_descendant() {
    let fixture = fixture(None);
    let request = spec(
        &fixture,
        r#"
printf '%s\n' "$$" > "$1"
trap '' TERM
/bin/sh -c 'trap "" TERM; printf "%s\n" "$$" > "$1"; while :; do sleep 1; done' fixture "$2" &
while :; do sleep 1; done
"#,
        Some(Duration::from_secs(30)),
        Some(SecretMaterial::new(vec![])),
    );
    let runtime = runtime();
    runtime.block_on(async {
        let running = tokio::spawn(ego_tools::exec::run_protocol_child(request, 1024));
        let deadline = Instant::now() + OBSERVATION_WINDOW;
        while recorded_pid(&fixture.descendant).is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            recorded_pid(&fixture.direct).is_some() && recorded_pid(&fixture.descendant).is_some(),
            "child and grandchild never recorded valid process IDs"
        );
        assert!(
            !running.is_finished(),
            "filter ended before runtime shutdown"
        );
        drop(running); // Keep the filter active until runtime shutdown.
    });
    let shutdown_started = Instant::now();
    drop(runtime);
    assert!(
        shutdown_started.elapsed() < Duration::from_secs(4),
        "runtime shutdown exceeded the bounded cleanup interval"
    );
    // The test process is still the shell's parent: a killed child can stay a
    // zombie until that parent exits, but it must not execute again.
    assert_not_running(&fixture.direct);
    assert_not_running(&fixture.descendant);
}

// Catches: dropping the generation future orphaning the filter supervisor and its process tree.
#[test]
fn protocol_filter_dropped_future_still_reaps_owned_tree() {
    let fixture = fixture(None);
    let request = spec(
        &fixture,
        tree_body(),
        None,
        Some(SecretMaterial::new(vec![])),
    );
    runtime().block_on(async {
        let running = tokio::spawn(ego_tools::exec::run_protocol_child(request, 1024));
        let deadline = Instant::now() + OBSERVATION_WINDOW;
        while !fixture.descendant.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(fixture.descendant.exists());
        running.abort();
        let _ = running.await;
        let direct = fixture.direct.clone();
        let descendant = fixture.descendant.clone();
        tokio::task::spawn_blocking(move || {
            assert_gone(&direct);
            assert_gone(&descendant);
        })
        .await
        .unwrap();
    });
}
