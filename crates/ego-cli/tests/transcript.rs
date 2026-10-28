//! Local transcript consumer contract. No provider traffic leaves loopback.
//! Capture once with EGO_CAPTURE_TRANSCRIPT=1 and --nocapture, then save the
//! TRANSCRIPT_CAPTURE JSON string verbatim as fixtures/transcript-v1.jsonl.

#![cfg(unix)]

mod support;

use std::io::BufRead;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};
use support::{Ego, FIXTURE_SESSION_ID, Places, Plan, Reply, Stub};

const CHECKPOINT: &str = "# Objective\nProduce a local transcript\n# Binding constraints and preferences\nLocal only.\n# Completed work and evidence\nThe tool returned transcript-tool-result.\n# Work in progress\nNone.\n# Decisions and rationale\nKeep the public transcript.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nShow the transcript.\n# Critical references\ndocs/reference/transcript.md";

fn records(bytes: &str) -> Vec<Value> {
    bytes
        .lines()
        .map(|line| serde_json::from_str(line).expect("complete JSONL record"))
        .collect()
}

fn export(places: &Places, session: &str, after: Option<u64>) -> String {
    let mut command = Ego::at(places.home_path(), places.workspace_path(), None)
        .subcommand("export")
        .args([session, "--format", "transcript"]);
    if let Some(after) = after {
        command = command.args(["--after", &after.to_string()]);
    }
    let result = command.run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert!(result.stderr.is_empty(), "{}", result.stderr);
    result.stdout
}

fn assert_contract(root: &[Value], successor: &[Value], checkpoint: &str) {
    assert_eq!(root[0]["type"], "transcript.header");
    assert_eq!(root[0]["v"], 1);
    assert_eq!(root[0]["session"], FIXTURE_SESSION_ID);
    assert!(root[0]["public_event_version"].is_u64());
    assert!(root[0]["lineage"].is_null());
    assert!(root[0]["compaction_predecessor"].is_null());
    let mut previous = 0;
    let mut ids = std::collections::HashSet::new();
    for record in &root[1..] {
        assert_eq!(record["type"], "transcript.event");
        assert_eq!(record["session"], root[0]["session"]);
        let seq = record["seq"].as_u64().unwrap();
        assert!(seq > previous, "session-local total order");
        previous = seq;
        assert!(ids.insert(record["id"].as_str().unwrap()));
        assert!(record["turn"].is_string());
        assert!(record.get("item").is_some());
        assert!(record.get("ts").is_some());
    }
    let payloads: Vec<_> = root.iter().skip(1).map(|r| &r["event"]).collect();
    assert!(
        payloads
            .iter()
            .any(|p| p["kind"] == "user_message" && p["text"] == "Produce a local transcript")
    );
    assert!(
        payloads
            .iter()
            .any(|p| p["kind"] == "agent_message" && p["text"] == "First answer")
    );
    assert!(
        payloads
            .iter()
            .any(|p| p["kind"] == "agent_message" && p["text"] == "Final answer")
    );
    let tool = root
        .iter()
        .find(|r| r["event"]["type"] == "item_started" && r["event"]["name"] == "exec")
        .expect("tool arguments preserved");
    assert!(
        tool["event"]["text"]
            .as_str()
            .unwrap()
            .contains("transcript-tool-result")
    );
    assert!(root.iter().any(|r| {
        r["item"] == tool["item"]
            && r["event"]["type"] == "item_completed"
            && r["event"]["ok"] == true
            && r["event"]["text"]
                .as_str()
                .is_some_and(|s| s.contains("transcript-tool-result"))
    }));
    assert!(
        payloads
            .iter()
            .any(|p| p["type"] == "goal_set" && p["objective"] == "Produce a local transcript")
    );
    assert!(
        payloads
            .iter()
            .any(|p| p["type"] == "goal_continuation" && p["text"].is_string())
    );
    assert!(
        payloads
            .iter()
            .any(|p| p["type"] == "goal_transitioned" && p["transition"]["kind"] == "achieved")
    );
    assert_eq!(successor[0]["type"], "transcript.header");
    assert_eq!(successor[0]["lineage"]["kind"], "compaction");
    assert_eq!(successor[0]["lineage"]["source"], root[0]["session"]);
    assert_eq!(successor[0]["compaction_predecessor"], root[0]["session"]);
    assert_ne!(successor[0]["session"], root[0]["session"]);
    let recorded_checkpoint = successor
        .iter()
        .find(|r| r["event"]["kind"] == "context_checkpoint")
        .expect("successor publishes its checkpoint");
    assert_eq!(recorded_checkpoint["event"]["text"], checkpoint);
    for record in root.iter().chain(successor) {
        let bytes = record.to_string();
        assert!(
            !bytes.contains("fixture-private-call-id"),
            "provider correlation is private"
        );
        assert!(!bytes.contains("reasoning_state"));
        assert!(!bytes.contains("provider_content"));
    }
}

/// Catches: local export drops lineage/goals/content or follow duplicates the
/// snapshot and loses events committed after attachment. The oracle is the
/// scripted provider/tool/checker output, observed through real CLI processes.
#[cfg(unix)]
#[test]
fn transcript_contract_preserves_chat_goals_lineage_and_follow_cursor() {
    let places = Places::new();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(support::tool_call(
                "fixture-private-call-id",
                "exec",
                r#"{"argv":["/bin/sh","-c","printf transcript-tool-result"],"timeout_ms":5000}"#,
            )),
            Reply::Sse(support::text("First answer")),
            Reply::Sse(support::text("Final answer")),
            Reply::Sse(support::text(CHECKPOINT)),
        ],
        ..Plan::default()
    });
    places.ego(&stub).home_file(
        "config.toml",
        "model = 'ollama/qwen3'\nthinking = 'off'\nsandbox = 'off'\n",
    );
    let marker = places.workspace_path().join("checked-once");
    let checker = places.file("check.sh", "if test -f \"$1\"; then\n printf '%s\\n' '{\"verdict\":\"achieved\",\"reason\":\"script verified\"}'\nelse\n touch \"$1\"\n printf '%s\\n' '{\"verdict\":\"continue\",\"reason\":\"continue once\"}'\nfi\n");
    let check = json!(["/bin/sh", checker, marker]).to_string();
    let run = places
        .ego(&stub)
        .args([
            "--session-id",
            FIXTURE_SESSION_ID,
            "--goal",
            "Produce a local transcript",
            "--goal-turns",
            "1",
            "--goal-check-command",
            &check,
            "--allow",
            "exec(/bin/sh *)",
            "Produce a local transcript",
        ])
        .run();
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let before = export(&places, FIXTURE_SESSION_ID, None);
    assert_eq!(
        before,
        export(&places, FIXTURE_SESSION_ID, None),
        "stable repeated snapshot"
    );
    let root = records(&before);
    let cursor = root.last().unwrap()["seq"].as_u64().unwrap();
    assert_eq!(
        records(&export(&places, FIXTURE_SESSION_ID, Some(cursor))).len(),
        1
    );

    // Observe a live append without inspecting the private session directory.
    let mut follower = Ego::at(places.home_path(), places.workspace_path(), None)
        .subcommand("export")
        .args([
            FIXTURE_SESSION_ID,
            "--format",
            "transcript",
            "--follow",
            "--after",
            &cursor.to_string(),
        ])
        .spawn();
    let stdout = follower.stdout();
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in stdout.lines() {
            if send.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let observed = || {
        let header = receive
            .recv_timeout(Duration::from_secs(10))
            .expect("follow header flushed");
        assert_eq!(records(&header)[0], root[0]);
        let changed = Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("resume")
            .args([
                FIXTURE_SESSION_ID,
                "--goal-operation",
                "set=Updated local objective",
                "--output",
                "json",
            ])
            .run();
        assert_eq!(changed.code, Some(0), "{}", changed.stderr);
        let line = receive
            .recv_timeout(Duration::from_secs(10))
            .expect("new goal event flushed");
        let event = &records(&line)[0];
        assert!(event["seq"].as_u64().unwrap() > cursor);
        assert_eq!(event["event"]["type"], "goal_set");
        assert_eq!(event["event"]["objective"], "Updated local objective");
        assert!(
            receive.recv_timeout(Duration::from_millis(600)).is_err(),
            "no duplicates while idle"
        );
    };
    // Always stop our follower, even if an assertion in the observation fails.
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(observed));
    let _ = follower.child.kill();
    let _ = follower.child.wait();
    reader.join().unwrap();
    observed.unwrap();

    let compacted = places
        .ego(&stub)
        .subcommand("compact")
        .args([FIXTURE_SESSION_ID, "--output", "json"])
        .run();
    assert_eq!(compacted.code, Some(0), "{}", compacted.stderr);
    let target = compacted.report()["target_session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let after = export(&places, &target, None);
    assert_contract(
        &root,
        &records(&after),
        &CHECKPOINT.replace("\nLocal only.\n", "\nNo pinned user constraints.\n"),
    );
    assert_eq!(
        stub.generations().len(),
        4,
        "only scripted loopback generations"
    );

    for flags in [
        vec!["--follow"],
        vec!["--after", "0"],
        vec!["--format", "transcript", "--sanitize"],
        vec!["--format", "transcript", "--file", "out"],
    ] {
        let command = Ego::at(places.home_path(), places.workspace_path(), None)
            .subcommand("export")
            .arg(FIXTURE_SESSION_ID);
        let refused = flags
            .into_iter()
            .fold(command, |command, flag| command.arg(flag))
            .run();
        assert_eq!(refused.code, Some(2), "invalid mode must refuse");
        assert!(refused.stdout.is_empty());
    }
    let fixture = format!("{before}{after}");
    if std::env::var_os("EGO_CAPTURE_TRANSCRIPT").is_some() {
        println!(
            "TRANSCRIPT_CAPTURE={}",
            serde_json::to_string(&fixture).unwrap()
        );
        let mut artifacts = serde_json::Map::new();
        artifacts.insert(
            ego_cli::reference::REFERENCE_PATH.into(),
            json!(ego_cli::reference::reference()),
        );
        for (shell, path) in ego_cli::reference::COMPLETIONS {
            artifacts.insert(
                (*path).into(),
                json!(ego_cli::reference::completion(*shell)),
            );
        }
        println!("TRANSCRIPT_ARTIFACTS={}", Value::Object(artifacts));
    } else {
        let fixture = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/transcript-v1.jsonl"
        ))
        .expect("recorded real loopback fixture");
        let golden = records(&fixture);
        let split = golden
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, r)| r["type"] == "transcript.header")
            .unwrap()
            .0;
        // This captured pre-ledger public transcript retains its historical text;
        // new compactions above must publish deterministic Binding instead.
        assert_contract(&golden[..split], &golden[split..], CHECKPOINT);
    }
}
