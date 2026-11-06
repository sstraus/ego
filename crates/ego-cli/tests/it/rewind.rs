//! `ego rewind ID --at ITEM_ID` against the real binary, a real Git
//! workspace and real builtin writes (`CLI.md` §6.7).

use std::path::Path;

use ego_core::event::domain::EventKind;
use ego_core::store::session_store::SessionEntry;
use serde_json::Value;

use crate::support::{FIXTURE_SESSION_ID, Places, Plan, Reply, Stub, text, tool_call};

const CONFIG: &str =
    "model = \"ollama/qwen3\"\nthinking = \"off\"\nsandbox = \"off\"\nmode = \"accept-edits\"\n";

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Rewind Test",
            "-c",
            "user.email=rewind@example.invalid",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(id: &str, path: &str, contents: &str) -> Reply {
    Reply::Sse(tool_call(
        id,
        "write",
        &serde_json::json!({"files": [{"path": path, "contents": contents}]}).to_string(),
    ))
}

/// Turn one writes `a.txt`; turn two rewrites it and creates `b.txt`.
/// Returns a message id of turn one.
fn two_turns(places: &Places) -> (Stub, String) {
    let workspace = places.workspace_path();
    git(workspace, &["init", "-q", "-b", "master"]);
    std::fs::write(workspace.join("a.txt"), "one\n").unwrap();
    git(workspace, &["add", "a.txt"]);
    git(workspace, &["commit", "-qm", "initial"]);
    let stub = Stub::start(Plan {
        completions: vec![
            write("w1", "a.txt", "two\n"),
            Reply::Sse(text("Turn one.")),
            Reply::Sse(tool_call(
                "w2",
                "write",
                &serde_json::json!({"files": [
                    {"path": "a.txt", "contents": "three\n"},
                    {"path": "b.txt", "contents": "new\n"}
                ]})
                .to_string(),
            )),
            Reply::Sse(text("Turn two.")),
        ],
        ..Plan::default()
    });
    places.ego(&stub).home_file("config.toml", CONFIG);
    let first = places
        .ego(&stub)
        .args(["--session-id", FIXTURE_SESSION_ID, "Change a"])
        .run();
    assert_eq!(first.code, Some(0), "stderr: {}", first.stderr);
    let second = places
        .ego(&stub)
        .subcommand("resume")
        .args([FIXTURE_SESSION_ID, "Change more"])
        .run();
    assert_eq!(second.code, Some(0), "stderr: {}", second.stderr);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "three\n"
    );
    assert!(workspace.join("b.txt").exists());
    // Catches: the journal keeps only the final file's checkpoint or before-image.
    let writes = events(places, FIXTURE_SESSION_ID);
    let evidence = writes
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Event(event) => match &event.kind {
                EventKind::ToolCallCompleted { result, .. } if result.file_writes().len() == 2 => {
                    Some(result.file_writes())
                }
                _ => None,
            },
            _ => None,
        })
        .expect("the batch evidence survives the restart");
    assert!(
        evidence
            .iter()
            .all(|file| file.checkpoint.is_some() && file.before_image.is_some())
    );
    assert_ne!(evidence[0].checkpoint, evidence[1].checkpoint);
    let message = events(places, FIXTURE_SESSION_ID)
        .into_iter()
        .find_map(|entry| match entry {
            SessionEntry::Event(event)
                if matches!(event.kind, EventKind::AssistantMessage { .. }) =>
            {
                event.env.item
            }
            _ => None,
        })
        .expect("turn one persisted an assistant message")
        .to_string();
    (stub, message)
}

fn events(places: &Places, session: &str) -> Vec<SessionEntry> {
    std::fs::read_to_string(
        places
            .home_path()
            .join("sessions")
            .join(session)
            .join("events.jsonl"),
    )
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str::<Value>(line).unwrap())
    .filter_map(|record| record.get("payload").cloned())
    .map(|payload| serde_json::from_value::<SessionEntry>(payload).unwrap())
    .collect()
}

fn sessions(places: &Places) -> usize {
    std::fs::read_dir(places.home_path().join("sessions"))
        .unwrap()
        .count()
}

// Catches: a rewind that forks the conversation but leaves the files of the
// later turn (a rewritten file stays rewritten, a created file stays), or
// that restores the turn's own write too.
#[test]
fn rewind_restores_the_files_of_later_turns_and_forks_after_the_turn() {
    let places = Places::new();
    let (stub, message) = two_turns(&places);
    let before = sessions(&places);

    let rewound = places
        .ego(&stub)
        .subcommand("rewind")
        .args([FIXTURE_SESSION_ID, "--at", message.as_str()])
        .run();

    assert_eq!(rewound.code, Some(0), "stderr: {}", rewound.stderr);
    let workspace = places.workspace_path();
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "two\n"
    );
    assert!(
        !workspace.join("b.txt").exists(),
        "the file turn two created remains"
    );
    assert!(
        rewound.stderr.contains("restored b.txt (delete)"),
        "{}",
        rewound.stderr
    );
    assert!(
        rewound.stderr.contains("restored a.txt (replace)"),
        "{}",
        rewound.stderr
    );
    let target = rewound.stdout.trim();
    assert_eq!(sessions(&places), before + 1);
    let listed = places
        .ego(&stub)
        .subcommand("ls")
        .args(["--output", "json"])
        .run();
    assert_eq!(listed.code, Some(0), "stderr: {}", listed.stderr);
    let row = listed.report()["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == target)
        .cloned()
        .expect("the new session is listed");
    assert_eq!(row["lineage"]["kind"], "fork", "{row}");
    assert_eq!(row["lineage"]["source"], FIXTURE_SESSION_ID, "{row}");
}

// Catches: a rewind overwriting an edit the user made outside ego after the
// turn, or restoring some files before it notices the conflict.
#[test]
fn an_external_change_after_the_turn_is_reported_and_nothing_is_rewound() {
    let places = Places::new();
    let (stub, message) = two_turns(&places);
    let workspace = places.workspace_path();
    std::fs::write(workspace.join("a.txt"), "edited by hand\n").unwrap();
    let before = sessions(&places);

    let refused = places
        .ego(&stub)
        .subcommand("rewind")
        .args([FIXTURE_SESSION_ID, "--at", message.as_str()])
        .run();

    assert_ne!(refused.code, Some(0), "stdout: {}", refused.stdout);
    assert!(
        refused.stderr.contains("changed outside ego"),
        "{}",
        refused.stderr
    );
    assert!(refused.stderr.contains("a.txt"), "{}", refused.stderr);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "edited by hand\n"
    );
    assert!(
        workspace.join("b.txt").exists(),
        "b.txt was restored before the refusal"
    );
    assert_eq!(sessions(&places), before, "a session was created");
}

// Catches: the pre-check reads only the newest checkpoint of each file, so a
// change outside ego between two later builtin writes of one file (an exec
// `sed`, a formatter, a hand edit) passes it; the newer restore is applied,
// the older one then conflicts, and the rewind fails with the file left at
// an intermediate state and no session. Running it again cannot finish,
// because the older checkpoint keeps conflicting.
#[test]
fn a_change_outside_ego_between_two_later_writes_is_reported_before_any_restore() {
    let places = Places::new();
    let workspace = places.workspace_path();
    git(workspace, &["init", "-q", "-b", "master"]);
    std::fs::write(workspace.join("a.txt"), "one\n").unwrap();
    git(workspace, &["add", "a.txt"]);
    git(workspace, &["commit", "-qm", "initial"]);
    let stub = Stub::start(Plan {
        completions: vec![
            write("w1", "a.txt", "two\n"),
            Reply::Sse(text("Turn one.")),
            write("w2", "a.txt", "three\n"),
            Reply::Sse(text("Turn two.")),
            write("w3", "a.txt", "four\n"),
            Reply::Sse(text("Turn three.")),
        ],
        ..Plan::default()
    });
    places.ego(&stub).home_file("config.toml", CONFIG);
    let first = places
        .ego(&stub)
        .args(["--session-id", FIXTURE_SESSION_ID, "Change a"])
        .run();
    assert_eq!(first.code, Some(0), "stderr: {}", first.stderr);
    let second = places
        .ego(&stub)
        .subcommand("resume")
        .args([FIXTURE_SESSION_ID, "Change a again"])
        .run();
    assert_eq!(second.code, Some(0), "stderr: {}", second.stderr);
    std::fs::write(workspace.join("a.txt"), "formatted outside ego\n").unwrap();
    let third = places
        .ego(&stub)
        .subcommand("resume")
        .args([FIXTURE_SESSION_ID, "Change a once more"])
        .run();
    assert_eq!(third.code, Some(0), "stderr: {}", third.stderr);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "four\n"
    );
    let message = events(&places, FIXTURE_SESSION_ID)
        .into_iter()
        .find_map(|entry| match entry {
            SessionEntry::Event(event)
                if matches!(event.kind, EventKind::AssistantMessage { .. }) =>
            {
                event.env.item
            }
            _ => None,
        })
        .expect("turn one persisted an assistant message")
        .to_string();
    let before = sessions(&places);

    let refused = places
        .ego(&stub)
        .subcommand("rewind")
        .args([FIXTURE_SESSION_ID, "--at", message.as_str()])
        .run();

    assert_ne!(refused.code, Some(0), "stdout: {}", refused.stdout);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "four\n",
        "a restore was applied before the refusal; stderr: {}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains("changed outside ego"),
        "{}",
        refused.stderr
    );
    assert_eq!(sessions(&places), before, "a session was created");
}

// Catches: failed batch completion loses an earlier checkpoint or lets rewind
// silently pass a possibly modified uncheckpointed failure.
#[test]
fn failed_multifile_write_survives_restart_and_blocks_unsafe_rewind() {
    let places = Places::new();
    let workspace = places.workspace_path();
    git(workspace, &["init", "-q", "-b", "master"]);
    std::fs::create_dir_all(workspace.join("directory")).unwrap();
    let stub = Stub::start(Plan {
        completions: vec![
            write("first", "a.txt", "before"),
            Reply::Sse(text("First turn.")),
            Reply::Sse(tool_call(
                "batch",
                "write",
                &serde_json::json!({"files": [
                    {"path": "a.txt", "contents": "after"},
                    {"path": "directory", "contents": "bad"},
                    {"path": "later.txt", "contents": "not written"}
                ]})
                .to_string(),
            )),
            Reply::Sse(text("Second turn.")),
        ],
        ..Plan::default()
    });
    places.ego(&stub).home_file("config.toml", CONFIG);
    let first = places
        .ego(&stub)
        .args(["--session-id", FIXTURE_SESSION_ID, "First"])
        .run();
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    let second = places
        .ego(&stub)
        .subcommand("resume")
        .args([FIXTURE_SESSION_ID, "Second"])
        .run();
    assert_eq!(second.code, Some(0), "{}", second.stderr);
    let saved = events(&places, FIXTURE_SESSION_ID);
    let result = saved
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Event(event) => match &event.kind {
                EventKind::ToolCallCompleted { ok: false, result }
                    if result.file_writes().len() == 2 =>
                {
                    Some(result)
                }
                _ => None,
            },
            _ => None,
        })
        .expect("failed batch and prior evidence are journaled");
    assert!(result.file_writes()[0].checkpoint.is_some());
    assert!(result.file_writes()[1].possibly_modified);
    assert!(result.file_writes()[1].checkpoint.is_none());
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "after"
    );
    assert!(!workspace.join("later.txt").exists());
    let message = saved
        .into_iter()
        .find_map(|entry| match entry {
            SessionEntry::Event(event)
                if matches!(event.kind, EventKind::AssistantMessage { .. }) =>
            {
                event.env.item
            }
            _ => None,
        })
        .unwrap()
        .to_string();
    let refused = places
        .ego(&stub)
        .subcommand("rewind")
        .args([FIXTURE_SESSION_ID, "--at", &message])
        .run();
    assert_ne!(refused.code, Some(0));
    assert!(
        refused.stderr.contains("saved no checkpoint"),
        "{}",
        refused.stderr
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "after"
    );
}

// Catches: a session run with checkpoints off rewound as if its later turns
// changed no file, so the conversation forks and the files stay changed.
#[test]
fn writes_without_checkpoints_after_the_turn_refuse_the_rewind() {
    let places = Places::new();
    let workspace = places.workspace_path();
    git(workspace, &["init", "-q", "-b", "master"]);
    let stub = Stub::start(Plan {
        completions: vec![
            write("w1", "a.txt", "two\n"),
            Reply::Sse(text("Turn one.")),
            write("w2", "a.txt", "three\n"),
            Reply::Sse(text("Turn two.")),
        ],
        ..Plan::default()
    });
    places.ego(&stub).home_file("config.toml", CONFIG);
    let first = places
        .ego(&stub)
        .args([
            "--no-checkpoints",
            "--session-id",
            FIXTURE_SESSION_ID,
            "Change a",
        ])
        .run();
    assert_eq!(first.code, Some(0), "stderr: {}", first.stderr);
    let second = places
        .ego(&stub)
        .subcommand("resume")
        .args([FIXTURE_SESSION_ID, "--no-checkpoints", "Change a again"])
        .run();
    assert_eq!(second.code, Some(0), "stderr: {}", second.stderr);
    let message = events(&places, FIXTURE_SESSION_ID)
        .into_iter()
        .find_map(|entry| match entry {
            SessionEntry::Event(event)
                if matches!(event.kind, EventKind::AssistantMessage { .. }) =>
            {
                event.env.item
            }
            _ => None,
        })
        .expect("turn one persisted an assistant message")
        .to_string();
    let before = sessions(&places);

    let refused = places
        .ego(&stub)
        .subcommand("rewind")
        .args([FIXTURE_SESSION_ID, "--at", message.as_str()])
        .run();

    assert_ne!(refused.code, Some(0), "stdout: {}", refused.stdout);
    assert!(
        refused.stderr.contains("saved no checkpoint"),
        "{}",
        refused.stderr
    );
    assert!(refused.stderr.contains("a.txt"), "{}", refused.stderr);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "three\n"
    );
    assert_eq!(sessions(&places), before, "a session was created");
}

// Catches: edit journals only one checkpoint, so restart and rewind leave one file changed.
#[test]
fn rewind_restores_both_native_edit_files_after_restart() {
    let places = Places::new();
    let workspace = places.workspace_path();
    git(workspace, &["init", "-q", "-b", "master"]);
    std::fs::write(workspace.join("a.txt"), "first old\n").unwrap();
    std::fs::write(workspace.join("b.txt"), "second old\n").unwrap();
    git(workspace, &["add", "a.txt", "b.txt"]);
    git(workspace, &["commit", "-qm", "initial"]);
    let stub = Stub::start(Plan {
        completions: vec![
        Reply::Sse(text("First turn.")),
        Reply::Sse(tool_call("edit-batch", "edit", &serde_json::json!({"files": [
            {"path":"a.txt", "chunks":[{"old_string":"first old", "new_string":"first new"}]},
            {"path":"b.txt", "chunks":[{"old_string":"second old", "new_string":"second new"}]}
        ]}).to_string())),
        Reply::Sse(text("Second turn.")),
    ],
        ..Plan::default()
    });
    places.ego(&stub).home_file("config.toml", CONFIG);
    let first = places
        .ego(&stub)
        .args(["--session-id", FIXTURE_SESSION_ID, "First"])
        .run();
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    let second = places
        .ego(&stub)
        .subcommand("resume")
        .args([FIXTURE_SESSION_ID, "--output", "jsonl", "Second"])
        .run();
    assert_eq!(second.code, Some(0), "{}", second.stderr);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "first new\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("b.txt")).unwrap(),
        "second new\n"
    );
    // Project the public event emitted by the real native turn, not invented tool args.
    let public: Vec<ego_core::public::PublicEvent> = second
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("public JSONL event"))
        .collect();
    let start = public.into_iter().find(|event| matches!(event,
        ego_core::public::PublicEvent::ItemStarted { name: Some(name), .. } if name == "edit"
    )).expect("native edit start event");
    let live = ego_acp::project::project_updates_v1(&start, workspace);
    // Load the persisted session with the production lifecycle and run controller.
    let store = std::sync::Arc::new(ego_cli::session::jsonl(
        places.home_path().join("sessions"),
        workspace.to_path_buf(),
    ));
    let clock = std::sync::Arc::new(ego_tools::clock::SystemClock);
    let executor = std::sync::Arc::new(ego_cli::run::CliRunExecutor::for_acp(
        None,
        ego_cli::cli::SessionShape::default(),
        store.clone(),
        std::sync::Arc::new(ego_core::evidence::EvidenceAdmissionService::new()),
    ));
    let runs = std::sync::Arc::new(ego_core::run::RunController::new(
        store.clone(),
        executor,
        clock.clone(),
        std::sync::Arc::new(ego_cli::host_ports::UuidIds),
        ego_core::run::RunRetention::new(std::num::NonZeroUsize::new(64).unwrap()).unwrap(),
    ));
    let lifecycle = ego_core::session_lifecycle::SessionLifecycle::new(store, runs, clock);
    let canonical_workspace = std::fs::canonicalize(workspace).unwrap();
    let attachment = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(lifecycle.load(FIXTURE_SESSION_ID.parse().unwrap(), &canonical_workspace))
        .expect("load persisted lifecycle replay");
    let replay = ego_acp::project::project_replay_v1(
        &attachment.replay,
        attachment.context_window,
        workspace,
    );
    for updates in [live, replay] {
        let wire = updates
            .iter()
            .map(|update| serde_json::to_value(update).unwrap())
            .find(|wire| wire["title"] == "edit a.txt, b.txt")
            .expect("edit projection");
        assert_eq!(wire["title"], "edit a.txt, b.txt");
        assert_eq!(
            wire["locations"][0]["path"],
            workspace.join("a.txt").to_str().unwrap()
        );
        assert_eq!(
            wire["locations"][1]["path"],
            workspace.join("b.txt").to_str().unwrap()
        );
        assert_eq!(wire["locations"].as_array().unwrap().len(), 2);
    }
    let saved = events(&places, FIXTURE_SESSION_ID);
    let evidence = saved
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Event(event) => match &event.kind {
                EventKind::ToolCallCompleted { ok: true, result }
                    if result.file_writes().len() == 2 =>
                {
                    Some(result.file_writes())
                }
                _ => None,
            },
            _ => None,
        })
        .expect("native edit evidence survives restart");
    assert!(
        evidence
            .iter()
            .all(|file| file.checkpoint.is_some() && file.before_image.is_some())
    );
    let message = saved
        .into_iter()
        .find_map(|entry| match entry {
            SessionEntry::Event(event)
                if matches!(event.kind, EventKind::AssistantMessage { .. }) =>
            {
                event.env.item
            }
            _ => None,
        })
        .unwrap()
        .to_string();
    let rewound = places
        .ego(&stub)
        .subcommand("rewind")
        .args([FIXTURE_SESSION_ID, "--at", &message])
        .run();
    assert_eq!(rewound.code, Some(0), "{}", rewound.stderr);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "first old\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("b.txt")).unwrap(),
        "second old\n"
    );
}
