use std::path::PathBuf;

use agent_client_protocol_schema::v1::{SessionUpdate, ToolCallStatus, ToolKind};
use ego_acp::project::{project_replay_v1, project_updates_v1, project_v1, project_v1_at};
use ego_core::event::id::{ItemId, TurnId};
use ego_core::public::{ItemKind, PublicEvent};
use ego_core::session_lifecycle::ReplayFrame;
use ego_core::state::SessionState;

// Pure projections need an absolute native path, not a Unix root-relative path.
fn workspace() -> PathBuf {
    std::env::temp_dir().join("ego-acp-projection-workspace")
}

fn started(name: &str, args: &str) -> PublicEvent {
    PublicEvent::ItemStarted {
        turn: TurnId::new(),
        item: ItemId::new(),
        kind: ItemKind::Tool,
        name: Some(name.to_owned()),
        text: Some(args.to_owned()),
    }
}

#[test]
fn exec_argv_is_visible_as_a_command_with_execution_kind() {
    let update = project_v1(&started("exec", r#"{"argv":["git","status"]}"#)).unwrap();
    let SessionUpdate::ToolCall(call) = update else {
        panic!("expected ACP tool call");
    };
    assert_eq!(call.title, "git status");
    assert_eq!(call.kind, ToolKind::Execute);
    assert_eq!(call.status, ToolCallStatus::Pending);
}

#[test]
fn file_tools_expose_absolute_locations_and_appropriate_kinds() {
    for (name, kind) in [
        ("read", ToolKind::Read),
        ("edit", ToolKind::Edit),
        ("write", ToolKind::Edit),
    ] {
        let args = serde_json::json!({"files": [{"path": workspace().join("src/main.rs")}]});
        let update = project_v1(&started(name, &args.to_string())).unwrap();
        let SessionUpdate::ToolCall(call) = update else {
            panic!("expected ACP tool call for {name}");
        };
        assert_eq!(call.kind, kind, "{name}");
        assert_eq!(call.locations.len(), 1, "{name}");
        assert_eq!(
            call.locations[0].path,
            workspace().join("src/main.rs"),
            "{name}"
        );
    }
}

// Catches: glob/grep falling through to Other in either live or replay projection.
#[test]
fn search_tools_keep_search_kind_on_the_live_and_replay_wire() {
    for name in ["glob", "grep"] {
        let event = started(name, r#"{"path":"src","query":"needle"}"#);
        let live = project_updates_v1(&event, &workspace());
        let replay = project_replay_v1(
            &[ReplayFrame {
                event,
                state: SessionState::default(),
                inherited: false,
            }],
            None,
            &workspace(),
            "projection-session",
        );
        for (source, updates) in [("live", live), ("replay", replay)] {
            let wire = serde_json::to_value(&updates[0]).unwrap();
            assert_eq!(wire["kind"], "search", "{name} {source}");
        }
    }
}

#[test]
fn relative_file_path_uses_the_admitted_session_root() {
    let update = project_v1_at(
        &started("read", r#"{"files":[{"path":"src/main.rs"}]}"#),
        &workspace(),
    )
    .unwrap();
    let SessionUpdate::ToolCall(call) = update else {
        panic!("expected ACP tool call");
    };
    assert_eq!(call.locations[0].path, workspace().join("src/main.rs"));
}

#[test]
fn tool_start_publishes_pending_then_in_progress_for_the_same_id() {
    let updates = project_updates_v1(
        &started("exec", r#"{"argv":["git","status"]}"#),
        &workspace(),
    );
    assert_eq!(updates.len(), 2);
    let SessionUpdate::ToolCall(first) = &updates[0] else {
        panic!("first update must create the call");
    };
    let SessionUpdate::ToolCallUpdate(second) = &updates[1] else {
        panic!("second update must advance the call");
    };
    assert_eq!(first.status, ToolCallStatus::Pending);
    assert_eq!(second.fields.status, Some(ToolCallStatus::InProgress));
    assert_eq!(first.tool_call_id, second.tool_call_id);
}

#[test]
fn tool_activity_is_marked_on_creation_and_progress_without_changing_acp_fields() {
    let updates = project_updates_v1(
        &started("exec", r#"{"argv":["git","status"]}"#),
        &workspace(),
    );
    let created = serde_json::to_value(&updates[0]).unwrap();
    let running = serde_json::to_value(&updates[1]).unwrap();
    assert_eq!(created["_meta"]["ego"]["salience"], "activity");
    assert_eq!(running["_meta"]["ego"]["salience"], "activity");
    assert_eq!(created["sessionUpdate"], "tool_call");
    assert_eq!(running["sessionUpdate"], "tool_call_update");
    assert_eq!(created["toolCallId"], running["toolCallId"]);

    for mut wire in [created, running] {
        wire.as_object_mut().unwrap().remove("_meta");
        let _: SessionUpdate = serde_json::from_value(wire)
            .expect("a client ignoring ego metadata still reads an ACP update");
    }
}

#[test]
fn reasoning_and_failed_tool_completion_remain_collapsible_activity() {
    let turn = TurnId::new();
    let reasoning = project_v1(&PublicEvent::ItemDelta {
        turn,
        item: ItemId::new(),
        kind: ItemKind::Reasoning,
        text: "checking the repository".to_owned(),
    })
    .unwrap();
    let failed_tool = project_v1(&PublicEvent::ItemCompleted {
        turn,
        item: ItemId::new(),
        kind: ItemKind::Tool,
        ok: false,
        text: Some("exit status 1".to_owned()),
    })
    .unwrap();
    for update in [reasoning, failed_tool] {
        let wire = serde_json::to_value(update).unwrap();
        assert_eq!(wire["_meta"]["ego"]["salience"], "activity", "{wire}");
        assert!(wire["_meta"]["ego"].get("action").is_none());
    }
}

#[test]
fn reconnect_replays_the_same_tool_progress_and_absolute_location() {
    let frames = [ReplayFrame {
        event: started("read", r#"{"files":[{"path":"src/main.rs"}]}"#),
        state: SessionState::default(),
        inherited: false,
    }];
    let updates = project_replay_v1(&frames, None, &workspace(), "projection-session");
    assert_eq!(updates.len(), 2);
    let first = serde_json::to_value(&updates[0]).unwrap();
    let second = serde_json::to_value(&updates[1]).unwrap();
    assert_eq!(
        first["locations"][0]["path"],
        workspace().join("src/main.rs").to_string_lossy().as_ref()
    );
    assert_eq!(second["status"], "in_progress");
    assert_eq!(first["toolCallId"], second["toolCallId"]);
}

#[test]
fn tool_completion_preserves_success_and_failure() {
    for (ok, expected) in [
        (true, ToolCallStatus::Completed),
        (false, ToolCallStatus::Failed),
    ] {
        let update = project_v1(&PublicEvent::ItemCompleted {
            turn: TurnId::new(),
            item: ItemId::new(),
            kind: ItemKind::Tool,
            ok,
            text: Some(if ok { "done" } else { "exit status 1" }.to_owned()),
        })
        .unwrap();
        let SessionUpdate::ToolCallUpdate(update) = update else {
            panic!("expected ACP tool call completion");
        };
        assert_eq!(update.fields.status, Some(expected));
    }
}

#[test]
fn acp_v1_wire_projection_keeps_fields_and_tolerates_bad_tool_input() {
    let read = project_updates_v1(
        &started("read", r#"{"files":[{"path":"src/main.rs"}]}"#),
        &workspace(),
    );
    let created = serde_json::to_value(&read[0]).unwrap();
    assert_eq!(created["sessionUpdate"], "tool_call");
    assert_eq!(created["title"], "read src/main.rs");
    assert_eq!(created["kind"], "read");
    assert_eq!(
        created["locations"][0]["path"],
        workspace().join("src/main.rs").to_string_lossy().as_ref()
    );
    assert_eq!(created["rawInput"]["files"][0]["path"], "src/main.rs");
    let running = serde_json::to_value(&read[1]).unwrap();
    assert_eq!(running["sessionUpdate"], "tool_call_update");
    assert_eq!(running["status"], "in_progress");
    assert_eq!(running["toolCallId"], created["toolCallId"]);

    let malformed = project_v1(&started("exec", "not JSON")).unwrap();
    let SessionUpdate::ToolCall(malformed) = malformed else {
        panic!("malformed input still names a tool");
    };
    assert_eq!(malformed.title, "exec");
    assert_eq!(malformed.kind, ToolKind::Execute);
    assert_eq!(malformed.raw_input, Some(serde_json::json!("not JSON")));
    assert!(malformed.locations.is_empty());

    println!("{}", serde_json::to_string(&read[0]).unwrap());
    println!("{}", serde_json::to_string(&read[1]).unwrap());
}

// Catches: ACP loses later paths or resolves relative paths outside the session root.
#[test]
fn file_batch_projects_all_locations_and_titles_for_each_tool() {
    for name in ["read", "write", "edit"] {
        let elsewhere = std::env::temp_dir().join("ego-acp-projection-elsewhere/b.rs");
        let args = serde_json::json!({"files": [{"path": "src/a.rs"}, {"path": elsewhere}]});
        let event = started(name, &args.to_string());
        let updates = project_updates_v1(&event, &workspace());
        let wire = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(
            wire["title"],
            format!("{name} src/a.rs, {}", elsewhere.display()),
            "{name}"
        );
        assert_eq!(
            wire["locations"][0]["path"],
            workspace().join("src/a.rs").to_string_lossy().as_ref(),
            "{name}"
        );
        assert_eq!(
            wire["locations"][1]["path"],
            elsewhere.to_string_lossy().as_ref(),
            "{name}"
        );
        assert_eq!(wire["locations"].as_array().unwrap().len(), 2, "{name}");
    }
}

// Catches: legacy ToolCallStarted records without provider ids lose replay correlation.
#[test]
fn legacy_tool_replay_correlates_by_durable_item_id() {
    use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
    use ego_core::event::id::{EventId, Producer, Seq};
    use ego_core::event::time::Timestamp;
    let mut seq = Seq::ZERO;
    let event = DomainEvent {
        env: Envelope {
            seq: seq.next(),
            id: EventId::new(),
            turn: TurnId::new(),
            item: Some("00000000-0000-7000-8000-000000000002".parse().unwrap()),
            parent: None,
            ts: Timestamp::from_millis(1_000),
            producer: Producer::Main,
        },
        kind: EventKind::ToolCallStarted {
            name: "ping".into(),
            args: "{}".into(),
            call_id: None,
        },
    };
    let mut state = SessionState::default();
    state.apply(&event);
    let frames = [ReplayFrame {
        event: ego_core::public::project_public(&event).unwrap(),
        state,
        inherited: false,
    }];
    let updates = project_replay_v1(
        &frames,
        None,
        &workspace(),
        "00000000-0000-7000-8000-000000000001",
    );
    let wire = serde_json::to_value(&updates[0]).unwrap();
    // Independent hashlib vector for the documented legacy tuple, not the production helper.
    assert_eq!(
        wire["_meta"]["tuic/call_id"],
        "41b2f3110aa4c4f4aa0ad353528ec86f7ca2a0d3ebe15f3e94b1c2465337c55a"
    );
    assert_eq!(wire["_meta"]["ego"]["salience"], "activity");
}
