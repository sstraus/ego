#![cfg(unix)]

//! Controlled local peers exercise the public framing and lifecycle boundary.
//! No provider, credentials, network, or installed filter is involved.

use std::ffi::OsString;
use std::time::{Duration, Instant};

use ego_core::credentials::SecretMaterial;
use ego_core::event::domain::CancelCause;
use ego_core::turn::CancelToken;
use ego_tools::exec::{SecretChildError, SecretChildRequest, start_protocol_session};

fn request(script: &str) -> SecretChildRequest {
    SecretChildRequest {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        cwd: None,
        stdin: None,
        cancel: CancelToken::new(),
        run_deadline: None,
        command_timeout: Duration::from_secs(2),
        termination_grace: Duration::from_millis(100),
    }
}

// Catches: spawning a fresh child for each frame loses stream-local state.
#[tokio::test]
async fn protocol_session_retains_child_state_across_exchanges_and_finishes() {
    let mut child = start_protocol_session(
        request("n=0; while IFS= read -r line; do n=$((n+1)); printf '{\"n\":%s}\\n' \"$n\"; done"),
        64,
    )
    .expect("start");
    let first = child
        .exchange(SecretMaterial::from("{}"))
        .await
        .expect("first");
    let second = child
        .exchange(SecretMaterial::from("{}"))
        .await
        .expect("second");
    assert_eq!(first.expose_secret_bytes(), br#"{"n":1}"#);
    assert_eq!(second.expose_secret_bytes(), br#"{"n":2}"#);
    child.finish().await.expect("successful exit and EOF");
}

// Catches: counting the newline against the cap rejects an exact-size frame.
#[tokio::test]
async fn protocol_session_frame_at_byte_limit_round_trips() {
    let mut child = start_protocol_session(request("exec /bin/cat"), 2).expect("start");
    assert_eq!(
        child
            .exchange(SecretMaterial::from("{}"))
            .await
            .expect("frame")
            .expose_secret_bytes(),
        b"{}",
    );
    child.finish().await.expect("finish");
}

// Catches: accepting oversized, unterminated, non-JSON, or surplus output as a valid exchange.
#[tokio::test]
async fn protocol_session_invalid_output_fails_closed() {
    for script in [
        "IFS= read -r line; printf '123\\n'; exec /bin/cat >/dev/null",
        "IFS= read -r line; printf '{}'; exit 0",
        "IFS= read -r line; printf 'xx\\n'; exec /bin/cat >/dev/null",
        "IFS= read -r line; printf '{}\\n{}\\n'; exec /bin/cat >/dev/null",
    ] {
        let mut child = start_protocol_session(request(script), 2).expect("start");
        let result = match child.exchange(SecretMaterial::from("{}")).await {
            Ok(_) => child.finish().await,
            Err(error) => Err(error),
        };
        assert!(matches!(result, Err(SecretChildError::InvalidRequest(_))));
    }
}

// Catches: framing multiple commands through one input or bypassing the input size bound.
#[tokio::test]
async fn protocol_session_invalid_input_is_rejected() {
    for input in ["{}\n{}", "123", "xx"] {
        let mut child = start_protocol_session(request("exec /bin/cat"), 2).expect("start");
        assert!(matches!(
            child.exchange(SecretMaterial::from(input)).await,
            Err(SecretChildError::InvalidRequest(_)),
        ));
    }
}

// Catches: finish only checks EOF and silently accepts a failing filter exit.
#[tokio::test]
async fn protocol_session_finish_requires_successful_exit() {
    let mut child = start_protocol_session(
        request("while IFS= read -r line; do printf '{}\\n'; done; exit 7"),
        2,
    )
    .expect("start");
    child
        .exchange(SecretMaterial::from("{}"))
        .await
        .expect("frame");
    assert!(matches!(
        child.finish().await,
        Err(SecretChildError::Exit(Some(7)))
    ));
}

// Catches: a timeout clock started at process launch expires while waiting for provider input.
#[tokio::test]
async fn protocol_session_idle_time_does_not_consume_exchange_timeout() {
    let mut req = request("exec /bin/cat");
    req.command_timeout = Duration::from_millis(200);
    let mut child = start_protocol_session(req, 2).expect("start");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        child
            .exchange(SecretMaterial::from("{}"))
            .await
            .expect("frame")
            .expose_secret_bytes(),
        b"{}"
    );
    child.finish().await.expect("finish");
}

// Catches: the owner waits forever for a child that accepts input but never returns a frame.
#[tokio::test]
async fn protocol_session_exchange_timeout_stops_unresponsive_child() {
    let mut req = request("IFS= read -r line; exec /bin/sleep 60");
    req.command_timeout = Duration::from_millis(200);
    let mut child = start_protocol_session(req, 2).expect("start");
    assert!(matches!(
        child.exchange(SecretMaterial::from("{}")).await,
        Err(SecretChildError::CommandTimeout)
    ));
}

// Catches: idle owners stop observing the run deadline and external cancellation.
#[tokio::test]
async fn protocol_session_idle_cancellation_and_run_deadline_remain_active() {
    for deadline in [false, true] {
        let mut req = request("exec /bin/cat");
        let cancel = req.cancel.clone();
        if deadline {
            req.run_deadline = Some(Instant::now() + Duration::from_millis(100));
        }
        let mut child = start_protocol_session(req, 2).expect("start");
        if deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
        } else {
            cancel.cancel(CancelCause::Host);
        }
        let error = child
            .exchange(SecretMaterial::from("{}"))
            .await
            .expect_err("stopped");
        assert!(if deadline {
            matches!(error, SecretChildError::RunTimeout)
        } else {
            matches!(error, SecretChildError::Cancelled)
        });
    }
}

fn wait_for_pid(path: &std::path::Path) -> i32 {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(pid) = std::fs::read_to_string(path)
            && let Ok(pid) = pid.trim().parse()
        {
            return pid;
        }
        assert!(Instant::now() < end, "fixture did not report its process");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_absence(pid: i32) {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        ) {
            return;
        }
        assert!(Instant::now() < end, "owned process was not reaped");
        std::thread::sleep(Duration::from_millis(5));
    }
}

// Catches: child cleanup lives on the caller runtime and is lost when that runtime shuts down.
#[test]
fn protocol_session_dropped_future_and_runtime_still_reap_child() {
    let temp = tempfile::tempdir().expect("temp");
    let marker = temp.path().join("pid");
    let mut req = request("printf '%s' \"$$\" >\"$1\"; IFS= read -r line; exec /bin/sleep 60");
    req.argv
        .extend([OsString::from("fixture"), marker.as_os_str().to_owned()]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut child = start_protocol_session(req, 64).expect("start without ambient runtime");
    let pid = wait_for_pid(&marker);
    runtime.block_on(async {
        let mut exchange = child.exchange(SecretMaterial::from("{}"));
        tokio::select! {
            result = &mut exchange => panic!("unexpected early exchange result: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        drop(exchange);
    });
    drop(runtime);
    wait_for_absence(pid);
    drop(child);
}

// Catches: handle drop closes a channel but leaves an idle child tree alive.
#[test]
fn protocol_session_handle_drop_without_caller_runtime_reaps_child() {
    let temp = tempfile::tempdir().expect("temp");
    let marker = temp.path().join("pid");
    let mut req = request("printf '%s' \"$$\" >\"$1\"; exec /bin/sleep 60");
    req.argv
        .extend([OsString::from("fixture"), marker.as_os_str().to_owned()]);
    let child = start_protocol_session(req, 64).expect("start");
    let pid = wait_for_pid(&marker);
    drop(child);
    wait_for_absence(pid);
}

// Catches: finish has no timeout when a filter ignores closed stdin.
#[tokio::test]
async fn protocol_session_finish_timeout_stops_child_that_ignores_eof() {
    let mut req = request("while IFS= read -r line; do printf '{}\\n'; done; exec /bin/sleep 60");
    req.command_timeout = Duration::from_millis(200);
    let mut child = start_protocol_session(req, 2).expect("start");
    child
        .exchange(SecretMaterial::from("{}"))
        .await
        .expect("frame");
    assert!(matches!(
        child.finish().await,
        Err(SecretChildError::CommandTimeout)
    ));
}

// Catches: initial stdin bypasses the exclusive bounded frame API.
#[test]
fn protocol_session_initial_stdin_is_rejected_before_launch() {
    let mut req = request("exit 0");
    req.stdin = Some(SecretMaterial::from("{}"));
    assert!(matches!(
        start_protocol_session(req, 64),
        Err(SecretChildError::InvalidRequest(_))
    ));
}

// Catches: inherited application environment exposes provider credentials to the filter.
#[tokio::test]
async fn protocol_session_only_exposes_the_minimal_environment() {
    let script = "while IFS= read -r line; do if [ \"$PATH\" = /usr/bin:/bin ] && [ -z \"${HOME+x}\" ] && [ -z \"${USER+x}\" ]; then printf 'true\\n'; else printf 'false\\n'; fi; done";
    let mut child = start_protocol_session(request(script), 64).expect("start");
    assert_eq!(
        child
            .exchange(SecretMaterial::from("{}"))
            .await
            .expect("frame")
            .expose_secret_bytes(),
        b"true"
    );
    child.finish().await.expect("finish");
}
