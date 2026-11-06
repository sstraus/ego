//! Real-process ACP shutdown after signal handlers have been installed by a turn.
#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};
use support::{AcpHost, Ego, Places, Plan, ReleaseGate, Reply, Stub, paced, text};

fn host(places: &Places, stub: &Stub) -> AcpHost {
    std::fs::write(
        places.home_path().join("config.toml"),
        "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"workspace\"\n",
    )
    .expect("isolated ACP configuration");
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(stub))
            .subcommand("acp")
            .spawn_conversing(),
    );
    host.initialize();
    host
}

fn terminate(host: &mut AcpHost) {
    let pid = nix::unistd::Pid::from_raw(i32::try_from(host.child().id()).expect("child PID"));
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).expect("signal owned ACP child");
    let start = Instant::now();
    loop {
        if let Some(status) = host.child().try_wait().expect("child status") {
            assert_eq!(
                status.code(),
                Some(143),
                "SIGTERM must use the documented exit code"
            );
            return;
        }
        if start.elapsed() >= Duration::from_secs(2) {
            host.child()
                .kill()
                .expect("kill only the timed-out owned child");
            host.child().wait().expect("reap timed-out child");
            panic!("idle ACP swallowed SIGTERM and did not exit within 2 s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn acp_exits_on_sigterm_when_idle() {
    // Catches: a completed turn leaves Tokio's global SIGTERM handler installed
    // with no ACP-level listener to consume subsequent termination signals.
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("ready"))],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    let cwd = places.workspace_path().to_str().expect("UTF-8 workspace");
    let session = host.new_session(cwd);
    let answer = host.request(
        "session/prompt",
        serde_json::json!({
            "sessionId": session, "prompt": [{"type": "text", "text": "Say ready"}],
        }),
    );
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    terminate(&mut host);
    host.finish();

    let mut resumed = self::host(&places, &stub);
    let loaded = resumed.request(
        "session/load",
        serde_json::json!({
            "sessionId": session, "cwd": cwd, "mcpServers": [],
        }),
    );
    assert!(
        loaded.get("result").is_some(),
        "session lease was not released: {loaded}"
    );
    resumed.finish();
}

#[test]
fn acp_sigterm_without_a_prompt_does_not_wait_for_stdin_eof() {
    // Catches: Tokio runtime teardown waits forever for the host's open stdin pipe.
    let places = Places::new();
    let stub = Stub::start(Plan::default());
    let mut host = host(&places, &stub);
    terminate(&mut host);
    host.finish();
}

#[test]
fn acp_sigterm_settles_a_streaming_continuation_before_exit() {
    // Catches: continuation admission replaces the shutdown-owned cancellation
    // token, or runtime shutdown abandons the turn before its durable settlement.
    let places = Places::new();
    let Reply::Paced { head, tail, .. } = paced("stream started", Duration::ZERO) else {
        unreachable!("paced reply");
    };
    let gate = ReleaseGate::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("ready")),
            Reply::GatedPaced {
                head,
                tail,
                gate: gate.clone(),
            },
        ],
        ..Plan::default()
    });
    let mut host = host(&places, &stub);
    let cwd = places.workspace_path().to_str().expect("UTF-8 workspace");
    let session = host.new_session(cwd);
    let prompt = serde_json::json!({
        "sessionId": session, "prompt": [{"type": "text", "text": "Continue"}],
    });
    let first = host.request("session/prompt", prompt.clone());
    assert_eq!(first["result"]["stopReason"], "end_turn", "{first}");
    host.send_request("session/prompt", prompt);
    loop {
        let frame = host.read_frame();
        if frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
            break;
        }
    }
    // Releasing the fixture on unwind prevents a failing signal assertion from
    // leaving its provider response thread blocked.
    struct ReleaseOnDrop(std::sync::Arc<ReleaseGate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let release = ReleaseOnDrop(gate);
    terminate(&mut host);
    drop(release);
    host.finish();

    let log = std::fs::read_to_string(
        places
            .home_path()
            .join("sessions")
            .join(&session)
            .join("events.jsonl"),
    )
    .expect("durable session log");
    let completed = log
        .lines()
        .filter_map(|line| {
            let record =
                ego_core::store::codec::decode_line(line.as_bytes()).expect("valid store record");
            let ego_core::store::codec::StoreLine::Record(record) = record else {
                return None;
            };
            let entry: ego_core::store::session_store::SessionEntry =
                serde_json::from_str(record.payload.get()).expect("session entry");
            let ego_core::store::session_store::SessionEntry::Event(event) = entry else {
                return None;
            };
            match event.kind {
                ego_core::event::domain::EventKind::TurnCompleted { exit_reason, .. } => {
                    Some(exit_reason)
                }
                _ => None,
            }
        })
        .next_back()
        .expect("interrupted turn durably completed before process exit");
    assert_eq!(
        completed,
        ego_core::event::domain::ExitReason::Cancelled(ego_core::event::domain::CancelCause::Host)
    );
    let mut resumed = self::host(&places, &stub);
    let loaded = resumed.request(
        "session/load",
        serde_json::json!({
            "sessionId": session, "cwd": cwd, "mcpServers": [],
        }),
    );
    assert!(
        loaded.get("result").is_some(),
        "busy shutdown left the session unavailable: {loaded}"
    );
    resumed.finish();
}
