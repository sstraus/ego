use std::path::Path;

use agent_client_protocol_schema::v1::{SessionUpdate, ToolCallStatus, ToolKind};
use ego_acp::project::{project_replay_v1, project_updates_v1, project_v1, project_v1_at};
use ego_core::event::id::{ItemId, TurnId};
use ego_core::public::{ItemKind, PublicEvent};
use ego_core::session_lifecycle::ReplayFrame;
use ego_core::state::SessionState;

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
        let update = project_v1(&started(name, r#"{"path":"/workspace/src/main.rs"}"#)).unwrap();
        let SessionUpdate::ToolCall(call) = update else {
            panic!("expected ACP tool call for {name}");
        };
        assert_eq!(call.kind, kind, "{name}");
        assert_eq!(call.locations.len(), 1, "{name}");
        assert_eq!(
            call.locations[0].path,
            Path::new("/workspace/src/main.rs"),
            "{name}"
        );
    }
}

#[test]
fn relative_file_path_uses_the_admitted_session_root() {
    let update = project_v1_at(
        &started("read", r#"{"path":"src/main.rs"}"#),
        Path::new("/workspace"),
    )
    .unwrap();
    let SessionUpdate::ToolCall(call) = update else {
        panic!("expected ACP tool call");
    };
    assert_eq!(call.locations[0].path, Path::new("/workspace/src/main.rs"));
}

#[test]
fn tool_start_publishes_pending_then_in_progress_for_the_same_id() {
    let updates = project_updates_v1(
        &started("exec", r#"{"argv":["git","status"]}"#),
        Path::new("/workspace"),
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
fn reconnect_replays_the_same_tool_progress_and_absolute_location() {
    let frames = [ReplayFrame {
        event: started("read", r#"{"path":"src/main.rs"}"#),
        state: SessionState::default(),
    }];
    let updates = project_replay_v1(&frames, None, Path::new("/workspace"));
    assert_eq!(updates.len(), 2);
    let first = serde_json::to_value(&updates[0]).unwrap();
    let second = serde_json::to_value(&updates[1]).unwrap();
    assert_eq!(first["locations"][0]["path"], "/workspace/src/main.rs");
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
        &started("read", r#"{"path":"src/main.rs"}"#),
        Path::new("/workspace"),
    );
    let created = serde_json::to_value(&read[0]).unwrap();
    assert_eq!(created["sessionUpdate"], "tool_call");
    assert_eq!(created["title"], "read src/main.rs");
    assert_eq!(created["kind"], "read");
    assert_eq!(created["locations"][0]["path"], "/workspace/src/main.rs");
    assert_eq!(created["rawInput"]["path"], "src/main.rs");
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
