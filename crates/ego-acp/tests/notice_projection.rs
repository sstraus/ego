use agent_client_protocol_schema::v1::SessionUpdate;
use ego_acp::project::{AcpNotice, NoticeAction, project_notice_v1};

// Catches: ACP dropping the sole warning that a configured cap cannot cover unpriced spend.
#[test]
fn cost_anomaly_reaches_the_acp_client_as_visible_text() {
    use ego_acp::project::{project_v1, project_v2};
    use ego_core::event::id::TurnId;
    use ego_core::public::PublicEvent;

    let event = PublicEvent::CostAnomaly {
        turn: TurnId::new(),
        model: "openrouter/z-ai/glm-4.7-flash".to_owned(),
        estimated_upper_bound: None,
        cause: Some(ego_core::cost::PricingUnavailable::OfflineNoCache),
    };
    for update in [
        project_v1(&event).map(|update| serde_json::to_value(update).unwrap()),
        project_v2(&event).map(|update| serde_json::to_value(update).unwrap()),
    ] {
        let update = update.expect("cost anomalies must remain visible to ACP users");
        let text = update["content"]["text"].as_str().unwrap();
        assert!(text.contains("openrouter/z-ai/glm-4.7-flash"));
        assert!(text.contains("cap"));
        assert!(text.contains("offline with no cached price table"));
    }
}

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
fn replay_does_not_merge_distinct_failures_from_the_same_turn_into_one_card() {
    use ego_acp::project::project_replay_v1;
    use ego_core::event::id::TurnId;
    use ego_core::generator::types::ErrorKind;
    use ego_core::public::PublicEvent;
    use ego_core::session_lifecycle::ReplayFrame;

    // A failed provider attempt and a later failed attempt can both emit
    // item-less errors in one turn. ACP chunks with one message id append.
    let turn = TurnId::new();
    let frames = ["first provider failed", "fallback provider failed"]
        .into_iter()
        .map(|message| ReplayFrame {
            event: PublicEvent::Error {
                turn,
                item: None,
                message: message.to_owned(),
                kind: ErrorKind::Unclassified,
            },
            state: Default::default(),
            inherited: false,
        })
        .collect::<Vec<_>>();
    let updates = project_replay_v1(&frames, None, std::path::Path::new("/workspace"));
    assert_eq!(updates.len(), 2);
    let first = serde_json::to_value(&updates[0]).unwrap();
    let second = serde_json::to_value(&updates[1]).unwrap();
    assert_ne!(
        first["messageId"], second["messageId"],
        "distinct historical failures must not append into one ACP card"
    );
    assert_eq!(
        updates,
        project_replay_v1(&frames, None, std::path::Path::new("/workspace")),
        "reopening the same history must preserve each failure card's identity"
    );
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
