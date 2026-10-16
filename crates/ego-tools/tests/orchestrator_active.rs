use ego_core::config::{ConfigLayer, Mode as PermissionMode, Ruleset, Source, Trust, resolve};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::orchestrator::{Mode, SpawnReceipt};
use ego_core::permit::{CanonPath, Classification, Op, PolicyEngine, PrepareCtx, ToolResult};
use ego_core::store::session_store::{SessionEntry, SessionStore};
use ego_tools::fs::OsFs;
use ego_tools::orchestration_events::{ObservationPump, spawn_receipts};
use ego_tools::orchestrator::tools_with_mode;
use ego_tools::orchestrator_active::tool;
use ego_tools::store::{JsonlStore, OsIo};
use futures::{StreamExt, executor::block_on};
use serde_json::{Value, json};
use std::ffi::OsStr;

// Catches: default/config precedence accidentally enables active effects.
#[test]
fn orchestration_mode_defaults_observe_and_resolves_the_selected_profile() {
    assert_eq!(
        resolve(&[], Trust::Untrusted, None).unwrap().orchestration,
        Mode::Observe
    );
    let user = ConfigLayer::parse(
        Source::User,
        "[orchestration]\nmode='active'\n[profile.coordinator.orchestration]\nmode='observe'\n",
    )
    .unwrap();
    assert_eq!(
        resolve(std::slice::from_ref(&user), Trust::Trusted, None)
            .unwrap()
            .orchestration,
        Mode::Active
    );
    assert_eq!(
        resolve(&[user], Trust::Trusted, Some("coordinator"))
            .unwrap()
            .orchestration,
        Mode::Observe
    );
    assert!(ConfigLayer::parse(Source::User, "[orchestration]\nmode='bogus'\n").is_err());
}

// Catches: observation advertises mutations or direct invocation bypasses its mode.
#[test]
fn observe_omits_active_operations_and_refuses_direct_calls() {
    let home = tempfile::tempdir().unwrap();
    let ctx = PrepareCtx {
        cwd: CanonPath::new(std::fs::canonicalize(home.path()).unwrap()),
        resolver: &OsFs,
    };
    let registry = tools_with_mode(
        Some(OsStr::new("socket")),
        home.path(),
        Some("parent".into()),
        Mode::Observe,
    );
    assert_eq!(registry.len(), 2);
    assert!(
        registry
            .iter()
            .all(|t| t.descriptor().name != "orchestrator_active")
    );
    let disabled = tool(Mode::Observe, home.path(), Some("parent".into()));
    for request in [
        json!({"operation":"spawn","role":"sol","name":"child","cwd":home.path(),"brief":"Do the bounded task"}),
        json!({"operation":"say","session_id":"child","message":"Continue"}),
        json!({"operation":"land","path":home.path(),"branch":"feature","message":"Merge"}),
    ] {
        assert!(
            disabled
                .prepare(request, &ctx)
                .unwrap_err()
                .to_string()
                .contains("observe")
        );
    }
    assert!(tools_with_mode(None, home.path(), None, Mode::Active).is_empty());
}

// Catches: a spawn omits its managed header, routes the wrong role, or interpolates brief text.
#[test]
fn active_spawn_prepares_exact_tuic_request_with_the_standard_header() {
    let home = tempfile::tempdir().unwrap();
    let ctx = PrepareCtx {
        cwd: CanonPath::new(std::fs::canonicalize(home.path()).unwrap()),
        resolver: &OsFs,
    };
    let active = tool(Mode::Active, home.path(), Some("parent-identity".into()));
    let brief = "Implement the task; literal $(touch unsafe) and 'quotes'";
    let call = active.prepare(json!({"operation":"spawn","role":"sol","name":"child","cwd":home.path(),"brief":brief}), &ctx).unwrap();
    let [Op::Spawn { spec }] = call.ops() else {
        panic!("expected one exact spawn");
    };
    assert_eq!(spec.argv()[0], "tuic");
    let request: Value = serde_json::from_str(spec.argv()[3].to_str().unwrap()).unwrap();
    assert_eq!(request["action"], "spawn");
    assert_eq!(request["model"], "gpt-6.1-sol");
    let prompt = request["prompt"].as_str().unwrap();
    assert!(prompt.starts_with("Read and apply ~/.codex/SUBAGENT.md"));
    assert!(prompt.contains("Parent: tuic_session parent-identity"));
    assert!(prompt.contains("BUILD POLICY"));
    assert!(prompt.contains("exactly one final RESULT or BLOCKED"));
    assert!(prompt.ends_with(brief));
    assert!(active.prepare(json!({"operation":"spawn","role":"fable","name":"child","cwd":home.path(),"brief":brief}), &ctx).is_err());
}

// Catches: native land bypasses the existing High card under yolo/allow rules.
#[test]
fn active_land_is_always_classified_as_a_human_card() {
    let home = tempfile::tempdir().unwrap();
    let wrapper = home
        .path()
        .join("workspace/example-project/tools/land/tuic-land.sh");
    std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
    std::fs::write(&wrapper, "").unwrap();
    let root = CanonPath::new(std::fs::canonicalize(home.path()).unwrap());
    let ctx = PrepareCtx {
        cwd: root.clone(),
        resolver: &OsFs,
    };
    let call = tool(Mode::Active, home.path(), Some("parent".into())).prepare(json!({"operation":"land","path":home.path(),"branch":"feature","message":"Merge feature"}), &ctx).unwrap();
    let policy = PolicyEngine::new(root, PermissionMode::Yolo, Ruleset::default())
        .with_coordinator_profile(true);
    assert!(matches!(policy.classify(call), Classification::Ask(_)));
}

fn receipt(id: &str) -> SpawnReceipt {
    SpawnReceipt {
        session_id: id.into(),
        name: "owned-child".into(),
        cwd: "/owned/worktree".into(),
        prompt: "An admitted managed prompt".into(),
        spawned_at_ms: 1_000,
    }
}

// Catches: pump resends early, twice across restart, or after a child already started.
#[test]
fn recorded_idle_spawn_is_reserved_once_after_sixty_seconds_and_survives_restart() {
    let home = tempfile::tempdir().unwrap();
    let sessions: Value =
        serde_json::from_str(include_str!("fixtures/orchestrator/sessions.json")).unwrap();
    let idle = receipt("ed13937c-7aa9-5428-9cfa-c18ffb6c6498");
    let working = receipt("c7d80d16-5f27-5d3e-8363-9eca5cee4d7a");
    let missing = receipt("unknown-child");
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    assert!(
        pump.supervise_sessions(
            &[idle.clone(), working.clone(), missing.clone()],
            &sessions,
            60_999
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        pump.supervise_sessions(
            &[idle.clone(), working.clone(), missing.clone()],
            &sessions,
            61_000
        )
        .unwrap(),
        vec![idle.clone()]
    );
    drop(pump);
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    assert!(
        pump.supervise_sessions(&[idle, working, missing], &sessions, 120_000)
            .unwrap()
            .is_empty()
    );
}

// Catches: child ledger disappears on reopen or unrelated tool output forges supervision.
#[test]
fn native_spawn_receipt_is_projected_from_the_reopened_session_store_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::new(dir.path().join("sessions"), dir.path(), Box::new(OsIo));
    let session = SessionId::new();
    let item = ItemId::new();
    let turn = TurnId::new();
    let mut seq = Seq::ZERO;
    let mut event = |kind| DomainEvent {
        env: Envelope {
            seq: seq.next(),
            id: EventId::new(),
            turn,
            item: Some(item),
            parent: None,
            ts: Timestamp::from_millis(1_000),
            producer: Producer::Main,
        },
        kind,
    };
    let start = event(EventKind::ToolCallStarted { name: "orchestrator_active".into(), args: json!({"operation":"spawn","role":"sol","name":"owned-child","cwd":"/owned/worktree","brief":"Task"}).to_string(), call_id: None });
    let receipt = receipt("real-child-receipt");
    let done = event(EventKind::ToolCallCompleted {
        ok: true,
        result: ToolResult::builtin(json!({"orchestration_spawn":receipt}).to_string()),
    });
    store.append(&session, &start).unwrap();
    store.append(&session, &done).unwrap();
    drop(store);
    let reopened = JsonlStore::new(dir.path().join("sessions"), dir.path(), Box::new(OsIo));
    let entries = block_on(reopened.read(&session).unwrap().collect::<Vec<_>>())
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(spawn_receipts(&entries).unwrap(), vec![receipt]);
    assert!(
        spawn_receipts(&[SessionEntry::Event(done)])
            .unwrap()
            .is_empty()
    );
}

// Catches: restart resends a prompt whose fast child already wrote RESULT before its first tick.
#[test]
fn an_existing_result_file_prevents_resend_when_the_recorded_child_is_idle() {
    let home = tempfile::tempdir().unwrap();
    let results = home.path().join("Gits/.tmp/results");
    std::fs::create_dir_all(&results).unwrap();
    std::fs::write(
        results.join("owned-child-1000.md"),
        "RESULT: completed bounded task",
    )
    .unwrap();
    let sessions: Value =
        serde_json::from_str(include_str!("fixtures/orchestrator/sessions.json")).unwrap();
    let idle = receipt("ed13937c-7aa9-5428-9cfa-c18ffb6c6498");
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    pump.scan_results(1_000).unwrap(); // Previous observer had not projected native child rows yet.
    drop(pump);
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    assert!(
        pump.supervise_sessions(&[idle], &sessions, 61_000)
            .unwrap()
            .is_empty()
    );
}

// Catches: one resume issues more than two corrections, or rebuilding the tool resets the journal budget.
#[test]
fn resume_rework_budget_refuses_third_request_and_survives_rebinding() {
    use ego_tools::orchestrator_active::tool_with_rework_limits;
    let home = tempfile::tempdir().unwrap();
    let ctx = PrepareCtx {
        cwd: CanonPath::new(std::fs::canonicalize(home.path()).unwrap()),
        resolver: &OsFs,
    };
    let request =
        json!({"operation":"say", "session_id":"child", "message":"Fix the recorded failure"});
    let active = tool_with_rework_limits(
        Mode::Active,
        home.path(),
        Some("parent".into()),
        [("child".into(), 0)].into(),
    );
    assert!(active.prepare(request.clone(), &ctx).is_ok());
    assert!(active.prepare(request.clone(), &ctx).is_ok());
    assert!(
        active
            .prepare(request.clone(), &ctx)
            .unwrap_err()
            .to_string()
            .contains("two rework")
    );
    let rebound = tool_with_rework_limits(
        Mode::Active,
        home.path(),
        Some("parent".into()),
        [("child".into(), 2)].into(),
    );
    assert!(rebound.prepare(request, &ctx).is_err());
}

// Catches: reopened native-tool journal loses prior corrections or charges another child's thread.
#[test]
fn resume_rework_limits_come_from_owned_spawn_and_say_journal_events() {
    use ego_tools::orchestrator_active::rework_limits;
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::new(dir.path().join("sessions"), dir.path(), Box::new(OsIo));
    let session = SessionId::new();
    let item = ItemId::new();
    let mut seq = Seq::ZERO;
    let turn = TurnId::new();
    let mut append = |kind| {
        seq = seq.next();
        store
            .append(
                &session,
                &DomainEvent {
                    env: Envelope {
                        seq,
                        id: EventId::new(),
                        turn,
                        item: Some(item),
                        parent: None,
                        ts: Timestamp::from_millis(1_000),
                        producer: Producer::Main,
                    },
                    kind,
                },
            )
            .unwrap();
    };
    append(EventKind::ToolCallStarted { name: "orchestrator_active".into(), args: json!({"operation":"spawn","role":"sol","name":"owned-child","cwd":"/owned/worktree","brief":"Task"}).to_string(), call_id: None });
    append(EventKind::ToolCallCompleted {
        ok: true,
        result: ToolResult::builtin(
            json!({"orchestration_spawn":receipt("owned-child")}).to_string(),
        ),
    });
    for target in ["owned-child", "other-child", "owned-child"] {
        append(EventKind::ToolCallStarted {
            name: "orchestrator_active".into(),
            args: json!({"operation":"say","session_id":target,"message":"Fix"}).to_string(),
            call_id: None,
        });
    }
    drop(store);
    let reopened = JsonlStore::new(dir.path().join("sessions"), dir.path(), Box::new(OsIo));
    let entries = block_on(reopened.read(&session).unwrap().collect::<Vec<_>>())
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        rework_limits(&entries).unwrap(),
        [("owned-child".into(), 2)].into()
    );
}

// Catches: a settled child triggers another TUIC read/resend after observer restart.
#[test]
fn settled_spawn_supervision_needs_no_runtime_or_transport_after_restart() {
    let home = tempfile::tempdir().unwrap();
    let working = receipt("c7d80d16-5f27-5d3e-8363-9eca5cee4d7a");
    let sessions: Value =
        serde_json::from_str(include_str!("fixtures/orchestrator/sessions.json")).unwrap();
    let mut pump = ObservationPump::open(home.path(), home.path()).unwrap();
    assert!(
        pump.supervise_sessions(std::slice::from_ref(&working), &sessions, 61_000)
            .unwrap()
            .is_empty()
    );
    drop(pump);
    let mut reopened = ObservationPump::open(home.path(), home.path()).unwrap();
    // No Tokio runtime: any attempt to spawn a transport is itself a test failure.
    futures::executor::block_on(reopened.supervise(std::slice::from_ref(&working), 120_000))
        .unwrap();
    futures::executor::block_on(reopened.supervise(&[working], 180_000)).unwrap();
}
