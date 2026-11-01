use super::*;
use base64::Engine as _;

fn capabilities() -> crate::render::Capabilities {
    crate::render::Capabilities {
        osc: crate::render::OscProfile::Tuic,
        ..crate::render::Capabilities::plain()
    }
}

fn frames(bytes: &str) -> Vec<serde_json::Value> {
    bytes
        .split("\x1b]7770;agent-event=")
        .skip(1)
        .map(|part| {
            serde_json::from_slice(
                &base64::engine::general_purpose::STANDARD
                    .decode(part.split("\x1b\\").next().unwrap())
                    .unwrap(),
            )
            .unwrap()
        })
        .collect()
}

// Catches: modal answer, refusal, Escape, EOF or driver error leaves awaiting latched.
#[test]
fn cancelled_modal_retracts_native_wait() {
    for line in [
        Line::Submitted("continue".into()),
        Line::Blank,
        Line::Interrupted,
        Line::HostCommand(HostCommand::ModalEscape),
        Line::Ended,
        Line::ExternalBreak("driver stopped".into()),
    ] {
        let mut editor = Reedline::create();
        let phase = SurfacePhase::new(EditPhase::Idle);
        let status = Arc::new(Mutex::new(SurfaceStatus {
            signals: Some((Osc::new(capabilities()), true)),
            ..SurfaceStatus::default()
        }));
        let asked = AtomicBool::new(false);
        let (ingress, _control) = interactive_control();
        let session = SessionId::new();
        let mut drafts = DraftStash::new(session);
        let mut surface = ModalSurface {
            phase: &phase,
            status: &status,
            asked_for_image: &asked,
            ingress: &ingress,
            drafts: &mut drafts,
        };
        let (_sender, controls) = crossbeam::channel::unbounded();
        let (reply, mut answer) = oneshot::channel();
        let id = ItemId::new();
        TERMINAL_SCRIPT.set(Some(TerminalScript {
            lines: [line].into(),
            ..Default::default()
        }));
        assert!(answer_control(
            &mut editor,
            &phase,
            &mut surface,
            &controls,
            &mut None,
            Control::Question {
                id,
                request: QuestionRequest::new("Continue?", QuestionMode::FreeText, vec![], true),
                reply,
            }
        ));
        assert!(answer.try_recv().unwrap().is_some());
        let bytes = TERMINAL_SCRIPT.take().unwrap().signals.concat();
        assert!(bytes.contains("state=awaiting"));
        assert!(
            bytes.ends_with("\x1b]7770;state=idle\x07"),
            "must restore current idle: {bytes:?}"
        );
        assert_eq!(phase.get(), EditPhase::Idle);
        let frames = frames(&bytes);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["event"], "modal_open");
        assert_eq!(frames[0]["data"]["question"]["prompt"], "Continue?");
        assert_eq!(frames[1]["event"], "modal_end");
        assert_eq!(frames[0]["id"], id.to_string());
        assert_eq!(frames[1]["id"], frames[0]["id"]);
        assert_eq!(frames[0]["session"], session.to_string());
    }
}

// Catches: permission answer/rejection/cancel/EOF has no paired native settlement.
#[test]
fn permission_modal_settlement_restores_current_activity() {
    for line in [
        Line::Submitted("allow".into()),
        Line::Submitted("deny".into()),
        Line::Interrupted,
        Line::HostCommand(HostCommand::ModalEscape),
        Line::Ended,
        Line::ExternalBreak("driver error".into()),
    ] {
        let mut editor = Reedline::create();
        let status = Arc::new(Mutex::new(SurfaceStatus {
            signals: Some((Osc::new(capabilities()), true)),
            ..SurfaceStatus::default()
        }));
        let phase = SurfacePhase::new(EditPhase::Idle);
        let asked = AtomicBool::new(false);
        let (ingress, _control) = interactive_control();
        let mut drafts = DraftStash::new(SessionId::new());
        let mut surface = ModalSurface {
            phase: &phase,
            status: &status,
            asked_for_image: &asked,
            ingress: &ingress,
            drafts: &mut drafts,
        };
        let (_sender, controls) = crossbeam::channel::unbounded();
        let (reply, mut answer) = oneshot::channel();
        TERMINAL_SCRIPT.set(Some(TerminalScript {
            lines: [line].into(),
            ..Default::default()
        }));
        let request = PermissionRequest::new(
            "read",
            vec![],
            ego_core::permit::Escalation::NoRuleCovers {
                op: ego_core::permit::OpName::Opaque,
                path: ego_core::permit::CanonPath::new("/workspace"),
            },
        );
        assert!(answer_control(
            &mut editor,
            &phase,
            &mut surface,
            &controls,
            &mut None,
            Control::Permission { request, reply }
        ));
        assert!(answer.try_recv().unwrap().is_some());
        let bytes = TERMINAL_SCRIPT.take().unwrap().signals.concat();
        assert!(bytes.ends_with("\x1b]7770;state=idle\x07"));
        let frames = frames(&bytes);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["event"], "modal_open");
        assert_eq!(frames[1]["event"], "modal_end");
        assert_eq!(frames[0]["id"], frames[1]["id"]);
    }
}

// Catches: settlement restores activity saved on open, ignoring a newly running turn.
#[test]
fn native_modal_settlement_reads_current_activity() {
    let status = Arc::new(Mutex::new(SurfaceStatus {
        signals: Some((Osc::new(capabilities()), true)),
        ..SurfaceStatus::default()
    }));
    let phase = SurfacePhase::new(EditPhase::Idle);
    let asked = AtomicBool::new(false);
    let (ingress, _control) = interactive_control();
    let mut drafts = DraftStash::new(SessionId::new());
    let surface = ModalSurface {
        phase: &phase,
        status: &status,
        asked_for_image: &asked,
        ingress: &ingress,
        drafts: &mut drafts,
    };
    TERMINAL_SCRIPT.set(Some(TerminalScript::default()));
    let wait = native_wait::Wait::question(
        &surface,
        ItemId::new(),
        &QuestionRequest::new("Continue?", QuestionMode::FreeText, vec![], true),
    );
    status.lock().unwrap().begin_activity();
    wait.settle(&status, "answered");
    assert!(
        TERMINAL_SCRIPT
            .take()
            .unwrap()
            .signals
            .concat()
            .ends_with("\x1b]7770;state=busy\x07")
    );
}

// Catches: a modal returning after turn end emits a second clear or revives busy.
#[test]
fn native_turn_end_owns_the_last_modal_retraction() {
    use ego_core::event::id::TurnId;
    use ego_core::public::PublicEvent;
    let status = Arc::new(Mutex::new(SurfaceStatus {
        signals: Some((Osc::new(capabilities()), true)),
        ..SurfaceStatus::default()
    }));
    let turn = TurnId::new();
    status
        .lock()
        .unwrap()
        .observe_native_turn(&PublicEvent::TurnStarted { turn });
    status.lock().unwrap().begin_activity();
    let phase = SurfacePhase::new(EditPhase::Busy);
    let asked = AtomicBool::new(false);
    let (ingress, _control) = interactive_control();
    let mut drafts = DraftStash::new(SessionId::new());
    let surface = ModalSurface {
        phase: &phase,
        status: &status,
        asked_for_image: &asked,
        ingress: &ingress,
        drafts: &mut drafts,
    };
    TERMINAL_SCRIPT.set(Some(TerminalScript::default()));
    let wait = native_wait::Wait::question(
        &surface,
        ItemId::new(),
        &QuestionRequest::new("Continue?", QuestionMode::FreeText, vec![], true),
    );
    let before = TERMINAL_SCRIPT.with_borrow(|script| script.as_ref().unwrap().signals.clone());
    status
        .lock()
        .unwrap()
        .observe_native_turn(&PublicEvent::TurnCompleted {
            turn,
            ok: false,
            exit_reason: ego_core::event::domain::ExitReason::Cancelled(CancelCause::Host),
        });
    wait.settle(&status, "cancelled");
    assert_eq!(TERMINAL_SCRIPT.take().unwrap().signals, before);
}
