use agent_client_protocol_schema::v1::SessionUpdate;
use ego_acp::project::{AcpNotice, NoticeAction, project_notice_v1};

fn caps() -> ego_core::generator::capabilities::ModelCapabilities {
    use ego_core::generator::capabilities::*;
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        audio_input: AudioInputSupport::Unknown,
        max_context_tokens: None,
        max_output_tokens: None,
        max_tools: None,
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::Portable,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
    }
}

// Catches: a host notice delivered in the agent's answer stream.
#[test]
fn cost_anomaly_is_not_projected_as_agent_message_text() {
    use ego_acp::project::{project_acp_updates_v1, project_v2};
    use ego_core::event::id::TurnId;
    use ego_core::public::PublicEvent;
    use ego_core::state::SessionState;

    let event = PublicEvent::CostAnomaly {
        turn: TurnId::new(),
        model: "anthropic/claude-haiku-5-5".to_owned(),
        estimated_upper_bound: None,
        cause: None,
    };
    let live = project_acp_updates_v1(
        &event,
        &SessionState::default(),
        &caps(),
        std::path::Path::new("/workspace"),
    );
    let v2 = project_v2(&event).map(|update| serde_json::to_value(update).unwrap());
    let mut wires: Vec<serde_json::Value> = live
        .iter()
        .map(|update| serde_json::to_value(update).unwrap())
        .collect();
    wires.extend(v2);
    assert_eq!(wires.len(), 2, "one live v1 update and one v2 update");
    for wire in wires {
        assert_eq!(wire["sessionUpdate"], "session_info_update", "{wire}");
        assert!(wire.get("content").is_none(), "{wire}");
        let notice = &wire["_meta"]["ego"]["notice"];
        assert_eq!(notice["salience"], "card", "{wire}");
        assert_eq!(
            notice["text"],
            "Cost anomaly for anthropic/claude-haiku-5-5: no usable price is available; \
             any configured cap cannot cover this unpriced spend. Continuing."
        );
    }
}

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
        let text = update["_meta"]["ego"]["notice"]["text"].as_str().unwrap();
        assert!(text.contains("openrouter/z-ai/glm-4.7-flash"));
        assert!(text.contains("cap"));
        assert!(text.contains("offline with no cached price table"));
    }
}

#[test]
fn card_notice_is_a_session_info_notice_with_an_open_result_action() {
    let notice = AcpNotice {
        id: "result-42".to_owned(),
        text: "Worker finished: RESULT".to_owned(),
        action: NoticeAction::OpenResult {
            path: "/workspace/results/worker.md".into(),
        },
    };
    let wire = serde_json::to_value(project_notice_v1(&notice)).unwrap();
    assert_eq!(wire["sessionUpdate"], "session_info_update");
    assert!(wire.get("content").is_none(), "{wire}");
    let card = &wire["_meta"]["ego"]["notice"];
    assert_eq!(card["id"], "result-42");
    assert_eq!(card["text"], "Worker finished: RESULT");
    assert_eq!(card["salience"], "card");
    assert_eq!(card["action"]["kind"], "open_result");
    assert_eq!(card["action"]["path"], "/workspace/results/worker.md");

    let parsed: SessionUpdate = serde_json::from_value(wire)
        .expect("a client without ego support still receives a valid ACP update");
    assert!(matches!(parsed, SessionUpdate::SessionInfoUpdate(_)));
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
    let updates = project_replay_v1(
        &frames,
        None,
        std::path::Path::new("/workspace"),
        "projection-session",
    );
    assert_eq!(updates.len(), 2);
    let first = serde_json::to_value(&updates[0]).unwrap();
    let second = serde_json::to_value(&updates[1]).unwrap();
    assert_ne!(
        first["_meta"]["ego"]["notice"]["id"], second["_meta"]["ego"]["notice"]["id"],
        "distinct historical failures must not append into one ACP card"
    );
    assert_eq!(
        updates,
        project_replay_v1(
            &frames,
            None,
            std::path::Path::new("/workspace"),
            "projection-session"
        ),
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
    let updates = project_replay_v1(
        &frames,
        None,
        std::path::Path::new("/workspace"),
        "projection-session",
    );
    assert_eq!(updates.len(), 1);
    let wire = serde_json::to_value(&updates[0]).unwrap();
    assert_eq!(wire["sessionUpdate"], "session_info_update");
    assert_eq!(wire["_meta"]["ego"]["notice"]["salience"], "card");
    assert_eq!(
        wire["_meta"]["ego"]["notice"]["text"],
        "confinement changed before execution; the session must be reinitialized"
    );
    assert_eq!(
        updates,
        project_replay_v1(
            &frames,
            None,
            std::path::Path::new("/workspace"),
            "projection-session"
        )
    );
}

// Catches: child-only replay frames shift ordinary clients' error-card message IDs.
#[test]
fn child_frames_do_not_change_standard_replay_bytes() {
    use ego_acp::project::project_replay_v1;
    use ego_core::event::id::{ItemId, TurnId};
    use ego_core::generator::types::ErrorKind;
    use ego_core::public::{ChildIdentity, ChildPhase, PublicEvent};
    use ego_core::session_lifecycle::ReplayFrame;

    let turn = TurnId::new();
    let frame = |event| ReplayFrame {
        event,
        state: Default::default(),
        inherited: false,
    };
    let baseline = [frame(PublicEvent::Error {
        turn,
        item: None,
        message: "provider failed".to_owned(),
        kind: ErrorKind::Unclassified,
    })];
    let root = std::path::Path::new("/workspace");
    let baseline_bytes = serde_json::to_vec(&project_replay_v1(
        &baseline,
        None,
        root,
        "projection-session",
    ))
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&baseline_bytes).unwrap()[0]["_meta"]["ego"]["notice"]
            ["id"],
        format!("replay-error-{turn:?}-None-0")
    );
    let child = ChildIdentity::Foreground {
        turn: TurnId::new(),
    };
    let item = ItemId::new();
    for inherited in [false, true] {
        for count in [1, 2, 3] {
            let mut frames = Vec::new();
            for _ in 0..count {
                let mut start = frame(PublicEvent::ChildLifecycle {
                    turn,
                    item,
                    child,
                    phase: ChildPhase::Started,
                });
                start.inherited = inherited;
                frames.push(start);
            }
            frames.extend_from_slice(&baseline);
            let actual = serde_json::to_vec(&project_replay_v1(
                &frames,
                None,
                root,
                "projection-session",
            ))
            .unwrap();
            assert_eq!(
                actual, baseline_bytes,
                "{count} child frames, inherited={inherited}, changed standard replay bytes"
            );
        }
    }
}

// Catches: local filter text becoming an ACP agent message or model instruction.
#[test]
fn filter_notice_never_projects_as_an_acp_conversation_update() {
    let notice = ego_core::public::PublicEvent::FilterNotice {
        turn: ego_core::event::id::TurnId::new(),
        text: "Local policy is active".into(),
    };
    assert!(ego_acp::project::project_v1(&notice).is_none());
    assert!(ego_acp::project::project_v2(&notice).is_none());
    let public = serde_json::to_value(&notice).unwrap();
    assert_eq!(public["text"], "Local policy is active");
}

// Catches: live projection returns None for a hook PublicEvent::Error, so the
// notice reaches the ACP host only after a reload.
#[test]
fn live_projection_emits_hook_error_notice() {
    use ego_acp::project::project_acp_updates_v1;
    use ego_core::event::id::TurnId;
    use ego_core::generator::types::ErrorKind;
    use ego_core::public::PublicEvent;
    use ego_core::state::SessionState;

    let caps = caps();
    let root = std::path::Path::new("/workspace");
    let notice = "PreToolUse hook guard failed to run (observer): the hook handler failed";
    let event = |message: &str| PublicEvent::Error {
        turn: TurnId::new(),
        item: None,
        message: message.to_owned(),
        kind: ErrorKind::Unclassified,
    };
    // The reducer records a hook-produced error only in `errors`
    // (state.rs, EventKind::Error), and every other error in both lists.
    let mut state = SessionState::default();
    state.errors.push(notice.to_owned());
    let updates = project_acp_updates_v1(&event(notice), &state, &caps, root);
    assert_eq!(updates.len(), 1, "the hook notice must reach the host live");
    let wire = serde_json::to_value(&updates[0]).unwrap();
    assert_eq!(wire["sessionUpdate"], "session_info_update");
    assert_eq!(wire["_meta"]["ego"]["notice"]["text"], notice);
    assert_eq!(wire["_meta"]["ego"]["notice"]["salience"], "card");

    let failure = "provider refused the request";
    state.errors.push(failure.to_owned());
    state.non_hook_errors.push(failure.to_owned());
    assert!(
        project_acp_updates_v1(&event(failure), &state, &caps, root).is_empty(),
        "a turn failure belongs to the prompt RPC"
    );
}
