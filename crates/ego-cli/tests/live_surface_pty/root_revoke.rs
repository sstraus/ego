use super::*;

// Catches: revoke leaves an existing child or a future spawn holding a session root.
// Catches: treating a TERM trap's completion as required after forced termination.
#[test]
fn root_revoke_stops_the_running_child_and_denies_live_and_resumed_exec() {
    let (_outside, target, places) = external_write_grant("revoked.txt", b"2\r");

    let probe = |id: &str| {
        Reply::Sse(tool_call(
            id,
            "exec",
            &serde_json::json!({
                "argv": ["/bin/sh", "-c", format!("printf WRONG > '{}'", target.display())],
                "timeout_ms": 5000
            })
            .to_string(),
        ))
    };
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(tool_call("held-child", "exec", &serde_json::json!({
            "argv": ["/bin/sh", "-c", format!(
                "trap 'sleep 2; exit 0' TERM; printf CHILD-ACTIVE > '{}'; printf READY > ready; while :; do sleep 0.05; done", target.display())],
            "timeout_ms": 120000
        }).to_string())), probe("denied-live-exec"), Reply::Sse(text("PROBE-DONE-MARK"))],
        ..Plan::default()
    });
    let ego = places.ego(&stub).subcommand("resume").args([
        support::FIXTURE_SESSION_ID,
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input
        .write_all(b"start a child with the granted root\r")
        .unwrap();
    input.flush().unwrap();
    let ready = places.workspace_path().join("ready");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "granted child did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "CHILD-ACTIVE");
    #[cfg(unix)]
    let child_pids = {
        // Observe the host PID: Linux confinement gives the shell its own PID namespace.
        let processes = std::process::Command::new("ps")
            .args(["-axo", "pid=,args="])
            .output()
            .unwrap();
        assert!(processes.status.success());
        String::from_utf8(processes.stdout)
            .unwrap()
            .lines()
            .filter(|line| {
                line.contains("trap 'sleep 2;") && line.contains(target.to_str().unwrap())
            })
            .map(|line| {
                line.split_whitespace()
                    .next()
                    .unwrap()
                    .parse::<i32>()
                    .unwrap()
            })
            .collect::<Vec<_>>()
    };
    #[cfg(unix)]
    assert!(
        !child_pids.is_empty(),
        "the ready shell must have a host PID"
    );
    let start = output.len();
    input
        .write_all(format!("/config roots revoke {}\r", target.display()).as_bytes())
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Root revoked");
    until_prompt_from(&reader, &mut output, start);
    #[cfg(unix)]
    for child_pid in child_pids {
        assert_eq!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(child_pid), None),
            Err(nix::errno::Errno::ESRCH),
            "revoke receipt must follow child termination: PID {child_pid}"
        );
    }
    let start = output.len();
    input.write_all(b"probe after revoke\r").unwrap();
    input.flush().unwrap();
    #[cfg(target_os = "macos")]
    {
        until_normalized(&reader, &mut output, start, "for this session");
        input.write_all(b"n").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "What should it do instead?");
        input.write_all(b"\r").unwrap();
        input.flush().unwrap();
    }
    until_normalized(&reader, &mut output, start, "PROBE-DONE-MARK");
    until_prompt_from(&reader, &mut output, start);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "CHILD-ACTIVE");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let resumed = Stub::start(Plan {
        completions: vec![
            probe("denied-resumed-exec"),
            Reply::Sse(text("RESUME-DENIED-MARK")),
        ],
        ..Plan::default()
    });
    let result = places
        .ego(&resumed)
        .subcommand("resume")
        .args([
            support::FIXTURE_SESSION_ID,
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "workspace",
            "--mode",
            "yolo",
            "probe after a fresh resume",
        ])
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "CHILD-ACTIVE");
    for (generations, id) in [
        (stub.generations(), "denied-live-exec"),
        (resumed.generations(), "denied-resumed-exec"),
    ] {
        let result = generations.last().unwrap()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
            .expect("actual denied exec result")["content"]
            .to_string();
        assert!(
            result.contains("Permission denied")
                || result.contains("Operation not permitted")
                || result.contains("Directory nonexistent"),
            "{result}"
        );
    }
}

// Catches: a journaled permanent answer approves a later, independently re-added root.
#[test]
fn stale_permanent_receipt_does_not_approve_a_removed_and_readded_root() {
    let (_outside, target, places) = external_write_grant("stale-permanent.txt", b"3\r");

    let empty = Stub::start(Plan::default());
    let removed = places
        .ego(&empty)
        .subcommand("roots")
        .arg("rm")
        .arg(target.to_str().unwrap())
        .run();
    assert_eq!(removed.code, Some(0), "{}", removed.stderr);
    // The grant was re-admitted in process, so the session observation holds
    // the root. A narrower resume records the removal before the re-add.
    let narrowed = Stub::start(Plan {
        completions: vec![Reply::Sse(text("NARROWED-MARK"))],
        ..Plan::default()
    });
    let result = places
        .ego(&narrowed)
        .subcommand("resume")
        .args([
            support::FIXTURE_SESSION_ID,
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "workspace",
            "--mode",
            "yolo",
        ])
        .arg("record the narrower perimeter")
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    let added = places
        .ego(&empty)
        .subcommand("roots")
        .args(["add", "--access", "read-write"])
        .arg(target.to_str().unwrap())
        .run();
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let resumed = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "stale-write",
                "write",
                &serde_json::json!({"files": [{
                    "path": target, "contents": "STALE-CONSENT-USED"
                }]})
                .to_string(),
            )),
            Reply::Sse(text("STALE-PROBE-DONE")),
        ],
        ..Plan::default()
    });
    let result = places
        .ego(&resumed)
        .subcommand("resume")
        .args([
            support::FIXTURE_SESSION_ID,
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "workspace",
            "--mode",
            "yolo",
        ])
        .arg("probe stale permanent authority")
        .run();
    assert_eq!(
        result.code,
        Some(3),
        "stale consent must require a fresh human decision: {}",
        result.stderr
    );
    assert!(
        resumed.generations().is_empty(),
        "stale consent started provider effects"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "ORIGINAL");
}

// Uses the actual terminal question, config adapter and session journal.
fn external_write_grant(
    name: &str,
    choice: &[u8],
) -> (tempfile::TempDir, std::path::PathBuf, support::Places) {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join(name);
    std::fs::write(&target, "ORIGINAL").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let places = support::Places::new();
    places.file("AGENTS.md", "# Revocation fixture\n");
    let grant = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "ask-write",
                "write",
                &serde_json::json!({"files": [{
                    "path": target, "contents": "UNEXPECTED"
                }]})
                .to_string(),
            )),
            Reply::Sse(text("GRANT-SAVED-MARK")),
        ],
        ..Plan::default()
    });
    let ego = places.ego(&grant).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"request the external write\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Permanent");
    input.write_all(choice).unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "GRANT-SAVED-MARK");
    super::root_grants::readmit_after_root_grant(&reader, &mut output, &mut input, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "ORIGINAL");

    (outside, target, places)
}

// Catches: config publication becomes authority before its receipt is committed.
#[test]
fn permanent_grant_crash_after_config_before_commit_denies_on_resume() {
    permanent_grant_crash_cut(false);
}

// Catches: the first durable receipt is already marked committed before config exists.
#[test]
fn permanent_grant_crash_after_pending_before_config_denies_on_resume() {
    permanent_grant_crash_cut(true);
}

fn permanent_grant_crash_cut(before_config: bool) {
    let (_outside, target, places) = external_write_grant("atomic-root.txt", b"3\r");
    let log = places
        .home_path()
        .join("sessions")
        .join(support::FIXTURE_SESSION_ID)
        .join("events.jsonl");
    let contents = std::fs::read_to_string(&log).unwrap();
    let mut prefix = String::new();
    let mut found_commit = false;
    for line in contents.split_inclusive('\n') {
        if line.contains("\"PermanentRootGranted\"") {
            found_commit = true;
            break;
        }
        prefix.push_str(line);
    }
    assert!(
        found_commit,
        "terminal approval did not journal a committed receipt"
    );
    if before_config {
        assert!(
            prefix.contains("\"PermanentRootGrantPending\""),
            "no pending receipt precedes config publication"
        );
        std::fs::remove_file(places.home_path().join("config.toml")).unwrap();
    }
    // Keep exactly the real, checksummed prefix at the crash cut, not a fabricated approval state.
    let mut file = std::fs::File::create(&log).unwrap();
    file.write_all(prefix.as_bytes()).unwrap();
    file.sync_all().unwrap();
    for resumed in [true, false] {
        let stub = Stub::start(Plan {
            completions: vec![
                Reply::Sse(tool_call(
                    "crash-write",
                    "write",
                    &serde_json::json!({"files": [{
                        "path": target, "contents": "HALF-GRANT-ACTIVATED"
                    }]})
                    .to_string(),
                )),
                Reply::Sse(text("CRASH-PROBE-DONE")),
            ],
            ..Plan::default()
        });
        let command = places.ego(&stub);
        let command = if resumed {
            command
                .subcommand("resume")
                .arg(support::FIXTURE_SESSION_ID)
        } else {
            command
        };
        let result = command
            .args([
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "workspace",
                "--mode",
                "yolo",
            ])
            .arg("probe incomplete permanent root")
            .run();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "ORIGINAL",
            "half-published grant reached an actual write: {}",
            result.stderr
        );
        if before_config {
            assert_eq!(result.code, Some(0), "{}", result.stderr);
            let calls = stub.generations();
            assert_eq!(
                calls.len(),
                2,
                "the denied tool result must reach the provider"
            );
            assert!(
                calls[1].to_string().contains("outside the workspace"),
                "{}",
                calls[1]
            );
        } else {
            assert_ne!(result.code, Some(0), "uncommitted config must stop startup");
            assert!(
                stub.generations().is_empty(),
                "no provider effects before committed authority"
            );
        }
    }
}
