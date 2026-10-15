use agent_client_protocol_schema::v1::SessionUpdate;
use ego_acp::project::{AcpNotice, NoticeAction, project_notice_v1};

#[test]
fn card_notice_is_a_standard_message_with_an_open_result_action() {
    let notice = AcpNotice {
        id: "result-42".to_owned(),
        text: "Worker finished: RESULT".to_owned(),
        action: NoticeAction::OpenResult {
            path: "/workspace/results/worker.md".into(),
        },
    };
    let wire = serde_json::to_value(project_notice_v1(&notice)).unwrap();
    assert_eq!(wire["sessionUpdate"], "agent_message_chunk");
    assert_eq!(wire["messageId"], "result-42");
    assert_eq!(wire["content"]["type"], "text");
    assert_eq!(wire["content"]["text"], "Worker finished: RESULT");
    assert_eq!(wire["_meta"]["ego"]["salience"], "card");
    assert_eq!(wire["_meta"]["ego"]["action"]["kind"], "open_result");
    assert_eq!(
        wire["_meta"]["ego"]["action"]["path"],
        "/workspace/results/worker.md"
    );

    let mut plain = wire;
    plain.as_object_mut().unwrap().remove("_meta");
    let parsed: SessionUpdate = serde_json::from_value(plain)
        .expect("a client without ego support still receives a valid ACP message");
    assert!(matches!(parsed, SessionUpdate::AgentMessageChunk(_)));
}

#[test]
fn replay_keeps_historical_failure_as_a_card_without_changing_live_errors() {
    use ego_acp::project::{project_replay_v1, project_v1};
    use ego_core::event::id::TurnId;
    use ego_core::generator::types::ErrorKind;
    use ego_core::public::PublicEvent;
    use ego_core::session_lifecycle::ReplayFrame;
    let error = PublicEvent::Error {
        turn: TurnId::new(),
        item: None,
        message: "confinement changed before execution; the session must be reinitialized"
            .to_owned(),
        kind: ErrorKind::Unclassified,
    };
    assert!(
        project_v1(&error).is_none(),
        "live failures belong to the prompt RPC"
    );
    let frames = vec![ReplayFrame {
        event: error,
        state: Default::default(),
        inherited: false,
    }];
    let updates = project_replay_v1(&frames, None, std::path::Path::new("/workspace"));
    assert_eq!(updates.len(), 1);
    let wire = serde_json::to_value(&updates[0]).unwrap();
    assert_eq!(wire["sessionUpdate"], "agent_message_chunk");
    assert_eq!(wire["_meta"]["ego"]["salience"], "card");
    assert_eq!(
        wire["content"]["text"],
        "confinement changed before execution; the session must be reinitialized"
    );
    assert_eq!(
        updates,
        project_replay_v1(&frames, None, std::path::Path::new("/workspace"))
    );
}
