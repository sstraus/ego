use super::*;
use crate::repl::policy_controls::PolicyControlRequest;
use crate::run::InteractiveSeat;
use crossterm::event::KeyEvent;
use std::io::{self, BufReader, Cursor, Read};

struct TestPermissionRules;

impl PermissionRuleApplication for TestPermissionRules {
    fn remember_allow(
        &self,
        _plan: ego_core::config::rule_management::RememberPlan,
    ) -> Result<String, crate::repl::policy_controls::PolicyControlError> {
        Ok("remembered".into())
    }
}

// The sequential adapter consumes BufRead input without constructing
// Reedline or probing terminal cursor state.
#[test]
fn sequential_reader_submits_text_after_lf() {
    let mut input = Cursor::new(b"hello\n".to_vec());

    assert_eq!(
        read_sequential_line(&mut input),
        Line::Submitted("hello".to_string())
    );
}

#[test]
fn sequential_reader_strips_exactly_the_crlf_terminator() {
    let mut input = Cursor::new(b"hello\r\n".to_vec());

    assert_eq!(
        read_sequential_line(&mut input),
        Line::Submitted("hello".to_string())
    );
}

#[test]
fn sequential_reader_maps_whitespace_only_input_to_blank() {
    let mut input = Cursor::new(b" \t \n".to_vec());

    assert_eq!(read_sequential_line(&mut input), Line::Blank);
}

#[test]
fn sequential_reader_submits_a_final_unterminated_line() {
    let mut input = Cursor::new(b"final line".to_vec());

    assert_eq!(
        read_sequential_line(&mut input),
        Line::Submitted("final line".to_string())
    );
}

#[test]
fn sequential_reader_maps_immediate_eof_to_ended() {
    let mut input = Cursor::new(Vec::<u8>::new());

    assert_eq!(read_sequential_line(&mut input), Line::Ended);
}

struct FailingReader;

impl Read for FailingReader {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("injected sequential read failure"))
    }
}

#[test]
fn sequential_reader_maps_a_read_error_to_ended() {
    let mut input = BufReader::new(FailingReader);

    assert_eq!(read_sequential_line(&mut input), Line::Ended);
}

#[test]
fn trust_accepts_only_an_explicit_yes_or_no() {
    for yes in ["y", "yes", " YES "] {
        assert_eq!(parse_trust(yes), Some(Trust::Trusted));
    }
    for no in ["n", "no", " NO "] {
        assert_eq!(parse_trust(no), Some(Trust::Untrusted));
    }
    for invalid in ["", "maybe", "allow", "1"] {
        assert_eq!(parse_trust(invalid), None, "{invalid:?}");
    }
}

#[test]
fn every_populated_statusline_preset_states_the_workspace_posture() {
    use ego_core::config::StatusSegmentName as Segment;

    for (id, label, segments) in STATUSLINE_PRESETS {
        let named = segments
            .iter()
            .map(|segment| segment.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        assert!(
            label == "Off" || label.ends_with(&named),
            "the `{id}` label promises something other than what it writes: {label}"
        );
    }

    for preset in ["default", "full"] {
        let (_, _, segments) = STATUSLINE_PRESETS
            .iter()
            .find(|(id, _, _)| *id == preset)
            .expect("a preset the picker offers");
        assert!(
            segments.contains(&Segment::Trust),
            "a session that starts untrusted in silence needs `{preset}` to say so"
        );
    }
}

#[test]
fn the_plain_permission_vocabulary_matches_the_immediate_keys() {
    for (raw, expected) in [
        ("allow", PermissionWord::Allow),
        ("deny", PermissionWord::Deny),
        ("amend", PermissionWord::Amend),
        (" A ", PermissionWord::Allow),
        ("d", PermissionWord::Deny),
        ("m", PermissionWord::Amend),
        ("e", PermissionWord::Amend),
        ("r", PermissionWord::Remember),
        ("remember", PermissionWord::Remember),
        ("yes", PermissionWord::Allow),
        ("no", PermissionWord::Deny),
    ] {
        assert_eq!(parse_permission(raw), Some(expected), "{raw:?}");
    }
    for invalid in ["", "approve", "cancel"] {
        assert_eq!(parse_permission(invalid), None, "{invalid:?}");
    }
}

#[test]
fn long_queue_status_uses_the_unsliced_indicator_path() {
    let status = Arc::new(Mutex::new(SurfaceStatus {
        queue: QueueStatus {
            pending_steers: 1,
            queued: 0,
            interrupt_pending: false,
        },
        drafts: 0,
        notice: None,
        ..SurfaceStatus::default()
    }));
    let waiting = Waiting::new(status);

    assert_eq!(waiting.render_prompt_left().as_ref(), "");
    let indicator = waiting.render_prompt_indicator(PromptEditMode::Emacs);
    assert_eq!(indicator.as_ref(), format!("{QUEUE_LABEL} | > ").as_str());
}

#[test]
fn busy_surface_names_work_and_its_escape_cancellation_affordance() {
    let status = Arc::new(Mutex::new(SurfaceStatus {
        activity: true,
        ..SurfaceStatus::default()
    }));
    let waiting = Waiting::new(status);

    assert_eq!(waiting.render_prompt_left().as_ref(), "");
    let indicator = waiting.render_prompt_indicator(PromptEditMode::Emacs);
    assert!(indicator.contains("thinking for 0s — Esc cancels\n"));
    assert!(indicator.ends_with("\n> "));
}

#[test]
fn failed_answer_output_does_not_claim_a_visible_reply() {
    use crate::output::{InteractiveOutput, TextOutput};

    let (sender, receiver) = bounded::<String>(1);
    drop(receiver);
    let output = SurfaceOutput::new(sender, CancelToken::new());
    let reply_started = Arc::clone(&output.reply_started);

    InteractiveOutput::new(output).answer("answer that cannot reach the terminal\n");

    assert!(!reply_started.load(Ordering::Acquire));
}

#[test]
fn prompt_redraw_uses_one_reply_phase_for_left_row_and_indicator() {
    let status = Arc::new(Mutex::new(SurfaceStatus {
        activity: true,
        ..SurfaceStatus::default()
    }));
    let waiting = Waiting::new(Arc::clone(&status));

    assert_eq!(waiting.render_prompt_left().as_ref(), "");
    assert!(
        waiting
            .render_prompt_indicator(PromptEditMode::Emacs)
            .contains("thinking for")
    );
    status
        .lock()
        .expect("surface status lock")
        .reply_started
        .store(true, Ordering::Release);
    assert_eq!(waiting.render_prompt_left().as_ref(), "");
    assert_eq!(
        waiting
            .render_prompt_indicator(PromptEditMode::Emacs)
            .as_ref(),
        "Esc cancels | > "
    );
}

#[test]
fn multiline_notice_stands_above_the_composer() {
    let status = Arc::new(Mutex::new(SurfaceStatus {
        notice: Some("Commands\n  /help  Show help".to_owned()),
        ..SurfaceStatus::default()
    }));
    let waiting = Waiting::new(status);

    assert_eq!(
        waiting
            .render_prompt_indicator(PromptEditMode::Emacs)
            .as_ref(),
        "Commands\n  /help  Show help\n> "
    );
}

#[test]
fn modal_prompt_does_not_embed_a_live_status_notice() {
    let status = Arc::new(Mutex::new(SurfaceStatus {
        notice: Some("unrelated status".to_owned()),
        ..SurfaceStatus::default()
    }));
    let prompt = Question::new("allow call? [y/n] ".to_owned(), status);

    assert_eq!(
        prompt
            .render_prompt_indicator(PromptEditMode::Emacs)
            .as_ref(),
        "allow call? [y/n] "
    );
}

#[test]
fn help_routes_to_scrollback_instead_of_becoming_a_multiline_prompt() {
    let mut editor = Reedline::create();
    let phase = SurfacePhase::new(EditPhase::Idle);
    let status = Arc::new(Mutex::new(SurfaceStatus::with_registry(Arc::new(
        ActionRegistry::final_surface(),
    ))));

    let line = apply_surface_effect(
        &mut editor,
        &phase,
        &status,
        SurfaceActionEffect::Help,
        false,
    );

    assert_eq!(
        line,
        Some(Line::HostCommand(HostCommand::Action(ActionId::Help)))
    );
    assert_eq!(status.lock().unwrap().notice, None);
}

#[test]
fn expand_reaches_the_host_and_leaves_no_status_notice() {
    let mut editor = Reedline::create();
    let phase = SurfacePhase::new(EditPhase::Idle);
    let status = Arc::new(Mutex::new(SurfaceStatus::with_registry(Arc::new(
        ActionRegistry::final_surface(),
    ))));

    let line = apply_surface_effect(
        &mut editor,
        &phase,
        &status,
        SurfaceActionEffect::Expand,
        false,
    );

    assert_eq!(
        line,
        Some(Line::HostCommand(HostCommand::Action(ActionId::Expand)))
    );
    assert_eq!(status.lock().unwrap().notice, None);
}

#[test]
fn typing_dismisses_a_local_result_while_the_turn_phase_is_busy() {
    let phase = SurfacePhase::new(EditPhase::Busy);
    let status = Arc::new(Mutex::new(SurfaceStatus {
        notice: Some("Commands\n  /help  Show help".to_owned()),
        ..SurfaceStatus::default()
    }));
    let mut mode = SurfaceEditMode::with_status(Emacs::default(), phase, Arc::clone(&status));
    let key = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
        KeyCode::Char('n'),
        KeyModifiers::NONE,
    )))
    .expect("key event");

    mode.parse_event(key);

    assert_eq!(status.lock().unwrap().notice, None);
}

#[test]
fn control_commands_do_not_dismiss_a_local_result() {
    let phase = SurfacePhase::new(EditPhase::Idle);
    let status = Arc::new(Mutex::new(SurfaceStatus {
        notice: Some("1 unsent draft".to_owned()),
        ..SurfaceStatus::default()
    }));
    let mut mode = SurfaceEditMode::with_status(Emacs::default(), phase, Arc::clone(&status));
    let eof = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
    )))
    .expect("key event");

    mode.parse_event(eof);

    assert_eq!(
        status.lock().unwrap().notice.as_deref(),
        Some("1 unsent draft")
    );
}

#[test]
fn permission_modal_decisions_are_immediate_and_only_explicit_keys_can_approve() {
    let phase = SurfacePhase::new(EditPhase::Modal);
    let status = Arc::new(Mutex::new(SurfaceStatus {
        permission_modal: true,
        ..SurfaceStatus::default()
    }));
    let mut mode = SurfaceEditMode::with_status(Emacs::default(), phase, Arc::clone(&status));
    let key = |code| {
        ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            .expect("key event")
    };

    assert_eq!(
        mode.parse_event(key(KeyCode::Char('y'))),
        ReedlineEvent::ExecuteHostCommand(format!("{PERMISSION_DECISION_COMMAND}allow"))
    );
    assert_eq!(
        mode.parse_event(
            ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
                KeyCode::Char('Y'),
                KeyModifiers::SHIFT,
            )))
            .expect("shifted key event")
        ),
        ReedlineEvent::ExecuteHostCommand(format!("{PERMISSION_DECISION_COMMAND}allow"))
    );
    assert_eq!(
        mode.parse_event(key(KeyCode::Char('n'))),
        ReedlineEvent::ExecuteHostCommand(format!("{PERMISSION_DECISION_COMMAND}deny"))
    );
    assert_eq!(
        mode.parse_event(key(KeyCode::Char('e'))),
        ReedlineEvent::ExecuteHostCommand(format!("{PERMISSION_DECISION_COMMAND}amend"))
    );
    assert_eq!(mode.parse_event(key(KeyCode::Enter)), ReedlineEvent::None);
    assert_eq!(
        mode.parse_event(
            ReedlineRawEvent::try_from(Event::Paste("y".into())).expect("paste event")
        ),
        ReedlineEvent::None
    );
    assert_eq!(
        mode.parse_event(key(KeyCode::Esc)),
        ReedlineEvent::ExecuteHostCommand(MODAL_ESCAPE_COMMAND.to_string())
    );
    assert_eq!(
        mode.parse_event(
            ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .expect("Ctrl-C event")
        ),
        ReedlineEvent::ExecuteHostCommand(MODAL_ESCAPE_COMMAND.to_string())
    );
    assert_eq!(
        mode.parse_event(key(KeyCode::Char('r'))),
        ReedlineEvent::ExecuteHostCommand(format!("{PERMISSION_DECISION_COMMAND}remember"))
    );
    status.lock().unwrap().permission_modal = false;
    assert_ne!(
        mode.parse_event(key(KeyCode::Char('y'))),
        ReedlineEvent::ExecuteHostCommand(format!("{PERMISSION_DECISION_COMMAND}allow")),
        "a key delivered after modal ownership ended must not approve"
    );
}

#[test]
fn absent_statusline_uses_the_default_but_explicit_off_stays_off() {
    use ego_core::config::{StatusSegmentName as Segment, StatuslineLayer};

    assert_eq!(
        effective_statusline_segments(None),
        vec![
            Segment::Model,
            Segment::Effort,
            Segment::Mode,
            Segment::Context,
            Segment::Cost,
            Segment::Queue,
            Segment::Drafts,
            Segment::Interrupt,
            Segment::Cwd,
            Segment::Git,
            Segment::Trust,
        ]
    );
    assert!(
        effective_statusline_segments(Some(&StatuslineLayer {
            segments: Some(Vec::new()),
        }))
        .is_empty()
    );
}

#[test]
fn help_uses_registry_rows_and_preserves_them_at_narrow_widths() {
    let registry = ActionRegistry::final_surface();
    let expected = registry
        .iter()
        .flat_map(|action| [action.name(), action.description_text()])
        .collect::<Vec<_>>();

    for width in [80, 60, 40] {
        let rendered = render_action_help(&registry, width);
        for section in [
            "Discover",
            "Model",
            "Control",
            "Session",
            "Integrations",
            "Surface",
        ] {
            assert!(
                rendered.lines().any(|line| line == section),
                "missing {section:?} section at width {width}:\n{rendered}"
            );
        }
        assert!(rendered.ends_with('─'));
        let compact = rendered
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        assert!(
            rendered
                .lines()
                .all(|line| crate::render::line::cells(line) <= width),
            "help exceeded {width} cells:\n{rendered}"
        );
        for value in &expected {
            let value = value
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>();
            assert!(
                compact.contains(&value),
                "registry value {value:?} was lost at width {width}:\n{rendered}"
            );
        }
    }
}

#[test]
fn reading_a_prebuffered_submission_settles_busy_after_idle() {
    let phase = SurfacePhase::new(EditPhase::Idle);
    let (input_sender, input) = bounded(CHANNEL_CAPACITY);
    input_sender
        .send(Line::Submitted("initial".to_string()))
        .unwrap();
    let (seat_control, _seat_control_receiver) = bounded(CHANNEL_CAPACITY);
    let (_ingress, coordinator) = interactive_control();
    let registry = Arc::new(ActionRegistry::final_surface());
    let seat = Seat {
        kind: SeatKind::Live(Box::new(LiveSeat {
            input,
            control: seat_control,
            activated: AtomicBool::new(false),
            break_signal: Arc::new(AtomicBool::new(false)),
            phase: phase.clone(),
            status: Arc::new(Mutex::new(SurfaceStatus::with_registry(Arc::clone(
                &registry,
            )))),
            git_worker: Mutex::new(None),
            activity_worker: Mutex::new(None),
            output_owner: Mutex::new(None),
            owner: Mutex::new(None),
        })),
        terminal: true,
        registry,
        coordinator: Mutex::new(Some(coordinator)),
    };

    assert_eq!(seat.read(), Line::Submitted("initial".to_string()));
    assert_eq!(phase.get(), EditPhase::Busy);
    if let SeatKind::Live(live) = &seat.kind {
        let status = live.status.lock().expect("surface status lock");
        status.reply_started.store(true, Ordering::Release);
    }
    seat.busy();
    if let SeatKind::Live(live) = &seat.kind {
        let status = live.status.lock().expect("surface status lock");
        assert!(
            status.reply_started.load(Ordering::Acquire),
            "re-entering busy during one turn must retain visible answer state"
        );
    }
}

#[test]
fn enabling_policy_controls_without_catalogue_leaves_permissions_disabled() {
    let (seat, _) = Seat::new(CancelToken::new()).expect("test seat");
    let registry = seat.action_registry();
    for action in [
        ActionId::Mode,
        ActionId::Plan,
        ActionId::Yolo,
        ActionId::Permissions,
    ] {
        assert!(matches!(
            registry.availability(action),
            Availability::Disabled { .. }
        ));
    }
    seat.enable_policy_controls(Arc::new(TestPermissionRules));
    for action in [ActionId::Mode, ActionId::Plan, ActionId::Yolo] {
        assert_eq!(registry.availability(action), Availability::Enabled);
    }
    assert!(matches!(
        registry.availability(ActionId::Permissions),
        Availability::Disabled { .. }
    ));
    seat.join();
}

#[test]
fn enter_and_leave_modal_restore_composer_cursor_origin_and_phase() {
    let mut editor = Reedline::create();
    editor.run_edit_commands(&[
        EditCommand::InsertString("draft text".to_owned()),
        EditCommand::MoveToPosition {
            position: 5,
            select: false,
        },
    ]);
    let phase = SurfacePhase::new(EditPhase::Busy);
    let status = Arc::new(Mutex::new(SurfaceStatus {
        origin: InputOrigin::BracketedPaste,
        ..SurfaceStatus::default()
    }));
    let saved = enter_modal(&mut editor, &phase, &status);
    assert_eq!(phase.get(), EditPhase::Modal);
    assert_eq!(editor.current_buffer_contents(), "");
    leave_modal(&mut editor, &phase, &status, saved, EditPhase::Busy);
    assert_eq!(editor.current_buffer_contents(), "draft text");
    assert_eq!(editor.current_insertion_point(), 5);
    assert_eq!(status.lock().unwrap().origin, InputOrigin::BracketedPaste);
    assert_eq!(phase.get(), EditPhase::Busy);
}

#[test]
fn surface_action_sink_routes_all_four_policy_controls() {
    let pending = Arc::new(Mutex::new(None));
    let mut sink = SurfaceActionSink {
        pending: Arc::clone(&pending),
    };
    for (action, effect) in [
        (ActionId::Mode, SurfaceActionEffect::Mode),
        (ActionId::Plan, SurfaceActionEffect::Plan),
        (ActionId::Yolo, SurfaceActionEffect::Yolo),
        (ActionId::Permissions, SurfaceActionEffect::Permissions),
    ] {
        assert_eq!(
            sink.invoke(LocalAction {
                id: action,
                busy: false
            }),
            LocalActionOutcome::Completed
        );
        let observed = pending.lock().unwrap();
        assert_eq!(
            observed.as_ref().map(std::mem::discriminant),
            Some(std::mem::discriminant(&effect))
        );
        drop(observed);
        pending.lock().unwrap().take();
    }
}

#[test]
fn regression_case_surface_sink_routes_name_and_goal() {
    let pending = Arc::new(Mutex::new(None));
    let mut sink = SurfaceActionSink {
        pending: Arc::clone(&pending),
    };
    for (id, effect) in [
        (ActionId::Name, SurfaceActionEffect::Name),
        (ActionId::Goal, SurfaceActionEffect::Goal),
    ] {
        assert_eq!(
            sink.invoke(LocalAction { id, busy: false }),
            LocalActionOutcome::Completed
        );
        assert_eq!(
            pending.lock().unwrap().as_ref().map(std::mem::discriminant),
            Some(std::mem::discriminant(&effect))
        );
        pending.lock().unwrap().take();
    }
}

#[test]
fn regression_case_surface_effect_maps_idle_and_refuses_busy() {
    let mut editor = Reedline::create();
    let phase = SurfacePhase::new(EditPhase::Idle);
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    assert!(matches!(
        apply_surface_effect(
            &mut editor,
            &phase,
            &status,
            SurfaceActionEffect::Name,
            false
        ),
        Some(Line::HostCommand(HostCommand::SessionMetadata(
            crate::cli::SessionMetadataOperation::Name(crate::cli::NameOperation::Show)
        )))
    ));
    assert!(matches!(
        apply_surface_effect(
            &mut editor,
            &phase,
            &status,
            SurfaceActionEffect::Goal,
            false
        ),
        Some(Line::HostCommand(HostCommand::SessionMetadata(
            crate::cli::SessionMetadataOperation::Goal(crate::cli::GoalOperation::Show)
        )))
    ));
    for effect in [SurfaceActionEffect::Name, SurfaceActionEffect::Goal] {
        status.lock().unwrap().notice = None;
        assert!(apply_surface_effect(&mut editor, &phase, &status, effect, true).is_none());
        assert!(
            status
                .lock()
                .unwrap()
                .notice
                .as_deref()
                .unwrap_or_default()
                .contains("unavailable while a turn is running")
        );
    }
}

#[test]
fn permissions_menu_uses_six_private_ids_and_operator_spellings() {
    assert_eq!(
        permission_menu(),
        [
            PermissionMenuId::List,
            PermissionMenuId::Allow,
            PermissionMenuId::Deny,
            PermissionMenuId::Revoke,
            PermissionMenuId::Remember,
            PermissionMenuId::Explain,
        ]
    );
    assert_eq!(
        permission_menu()
            .iter()
            .map(PermissionMenuId::as_str)
            .collect::<Vec<_>>(),
        ["list", "allow", "deny", "revoke", "remember", "explain"]
    );
}

#[test]
fn plan_surface_effect_emits_typed_policy_command_without_prompt() {
    let mut editor = Reedline::create();
    let phase = SurfacePhase::new(EditPhase::Idle);
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    let line = apply_surface_effect(
        &mut editor,
        &phase,
        &status,
        SurfaceActionEffect::Plan,
        false,
    )
    .expect("plan action emits a host command");
    assert_eq!(
        line,
        Line::HostCommand(HostCommand::Policy(PolicyControlRequest::SetMode(
            Mode::Plan
        )))
    );
    assert_eq!(editor.current_buffer_contents(), "");
    assert_eq!(phase.get(), EditPhase::Idle);
}

#[test]
fn source_target_source_activation_restores_source_drafts_in_lifo_order() {
    let source = SessionId::new();
    let target = SessionId::new();
    let mut drafts = DraftStash::new(source);
    drafts.push_active(ComposerDraft {
        text: "source-first".to_string(),
        cursor: 12,
        origin: InputOrigin::Typed,
    });
    drafts.push_active(ComposerDraft {
        text: "source-second".to_string(),
        cursor: 13,
        origin: InputOrigin::Typed,
    });

    drafts.activate(target);
    assert_eq!(drafts.active(), target);
    assert_eq!(drafts.active_count(), 0);
    assert_eq!(drafts.count(source), 2);
    assert_eq!(drafts.count(target), 0);
    drafts.push_active(ComposerDraft {
        text: "target-only".to_string(),
        cursor: 11,
        origin: InputOrigin::Typed,
    });
    assert_eq!(drafts.pop_active().unwrap().text, "target-only");

    drafts.activate(source);
    assert_eq!(drafts.active(), source);
    assert_eq!(drafts.active_count(), 2);
    assert_eq!(drafts.pop_active().unwrap().text, "source-second");
    assert_eq!(drafts.pop_active().unwrap().text, "source-first");
    assert_eq!(drafts.active_count(), 0);
}
