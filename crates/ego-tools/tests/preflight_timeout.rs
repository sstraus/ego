//! RED contract for the protocol-neutral secret-child adapter.
//!
//! Credential commands remain owned by `ego-cli`, while process-group
//! ownership and bounded cleanup belong to `ego-tools`. This test names the
//! final secret-child seam expected by the preflight ADR. It deliberately does
//! not use the ordinary tool executor: tool evidence and null stdin are not
//! valid for a secret-bearing protocol.

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

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "secret-child fixture did not write {path:?}");
}

fn assert_gone(path: &Path) {
    let pid = std::fs::read_to_string(path).expect("process marker");
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let alive = Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .status()
            .expect("kill utility")
            .success();
        if !alive {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("secret child {} survived cleanup", pid.trim());
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
    let result = runtime().block_on(run_secret_child(spec(
        &fixture,
        tree_body(),
        Some(Duration::from_millis(100)),
        None,
    )));

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
