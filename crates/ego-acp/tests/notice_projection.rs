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
