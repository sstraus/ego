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
fn statusline_presets_store_templates_with_one_prompt_slot() {
    // Catches presets losing the composer or failing to display their offered fields.
    use ego_core::cost::{Amount, CumulativeCost, CurrencyCode, Money};
    let mut state = SessionState::default();
    state.context.used_tokens = Some(58_000);
    state.context.window = Some(272_000);
    state.cost = CumulativeCost::Known {
        total: Money::new(
            "0.661".parse::<Amount>().unwrap(),
            CurrencyCode::new("USD").unwrap(),
        ),
    };
    let snapshot = SurfaceSnapshot::from_state_with_git(
        SessionId::new(),
        &state,
        SelectionFacts::new(
            ego_core::config::ModelRef("provider/model".into()),
            ego_core::generator::types::ThinkingLevel::Medium,
            Mode::Yolo,
        )
        .with_fast(true),
        SeatFacts::new(
            QueueStatus {
                pending_steers: 1,
                queued: 2,
                interrupt_pending: true,
            },
            3,
            "/workspace/ego",
            Trust::Untrusted,
        ),
        Some(GitFacts::new("feat/preset", true)),
    );
    for (id, _, template) in STATUSLINE_PRESETS {
        let status = Arc::new(Mutex::new(SurfaceStatus {
            native_status: true,
            width: 300,
            snapshot: Some(snapshot.clone()),
            template: super::super::status::StatusTemplate::new(template.into()),
            ..SurfaceStatus::default()
        }));
        repaint_status(&status);
        let waiting = Waiting::new(status);
        let output = format!(
            "{}preset-probe",
            waiting.render_prompt_indicator(PromptEditMode::Emacs)
        );
        let mut terminal = vt100::Parser::new(12, 300, 0);
        terminal.process(output.replace('\n', "\r\n").as_bytes());
        let visible = terminal.screen().contents();
        assert_eq!(
            visible.matches("> preset-probe").count(),
            1,
            "{id}: {visible}"
        );
        assert!(!visible.contains("statusline:"), "{id}: {visible}");
        for critical in ["queue=1/2", "drafts=3", "interrupt=yes"] {
            assert!(visible.contains(critical), "{id}: {visible}");
        }
        match id {
            "off" => assert_eq!(
                visible,
                "queue=1/2\ndrafts=3\ninterrupt=yes\n> preset-probe"
            ),
            "minimal" => assert!(
                visible.starts_with("provider/model · medium · yolo\n"),
                "{visible}"
            ),
            "default" | "full" => {
                for field in [
                    "model",
                    "medium",
                    "fast",
                    "yolo",
                    "58k/272k",
                    "$0.66",
                    "/w/ego",
                    "feat/preset",
                    "untrusted",
                ] {
                    assert!(visible.contains(field), "{id} omitted {field}: {visible}");
                }
            }
            _ => panic!("preset {id} needs a visible-output contract"),
        }
        assert_eq!(terminal.screen().cursor_position().1, 14, "{id}: {visible}");
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
    assert!(indicator.contains("Working 0s \x1b[2m· Esc to interrupt\x1b[22m\n"));
    assert!(indicator.ends_with("\n> "));
}

/// Catches: tool output being counted as model tokens, or thinking staying
/// active once the answer starts. Reasoning starts with a delta, not a start event.
#[test]
fn working_progress_tracks_only_model_text_and_current_reasoning() {
    use ego_core::public::{ItemKind, PublicEvent};
    let turn = ego_core::event::id::TurnId::new();
    let item = ego_core::event::id::ItemId::new();
    let mut status = SurfaceStatus::default();
    status.begin_activity();
    status.observe_generation_progress(&PublicEvent::ItemDelta {
        turn,
        item,
        kind: ItemKind::Reasoning,
        text: "thought".into(),
    });
    assert!(status.thinking_since.is_some());
    status.observe_generation_progress(&PublicEvent::ItemDelta {
        turn,
        item,
        kind: ItemKind::Tool,
        text: "tool output".into(),
    });
    assert_eq!(status.activity_output_chars, 7);
    status.observe_generation_progress(&PublicEvent::ItemDelta {
        turn,
        item,
        kind: ItemKind::AgentMessage,
        text: "answer".into(),
    });
    assert_eq!(status.activity_output_chars, 13);
    assert!(status.thinking_since.is_none());
    status.begin_activity();
    assert_eq!(
        status.activity_output_chars, 13,
        "same busy turn retains progress"
    );
    status.activity = false;
    status.begin_activity();
    assert_eq!(status.activity_output_chars, 0, "next turn starts fresh");
}

/// Catches: estimated output tokens being presented as an exact provider
/// count, or a live reasoning interval disappearing from the working row.
#[test]
fn busy_surface_labels_estimated_tokens_and_active_thinking() {
    let status = Arc::new(Mutex::new(SurfaceStatus {
        activity: true,
        activity_output_chars: 1_752,
        thinking_since: Some(Instant::now() - Duration::from_secs(1)),
        ..SurfaceStatus::default()
    }));
    let waiting = Waiting::new(status);
    let indicator = waiting.render_prompt_indicator(PromptEditMode::Emacs);
    assert!(indicator.contains("↓ ~438 tokens"), "{indicator}");
    assert!(indicator.contains("thinking started 1s ago"), "{indicator}");
}

// Catches: long turns shown as raw seconds, or losing the smaller unit at a boundary.
#[test]
fn working_indicator_formats_long_elapsed_time() {
    for (seconds, expected) in [
        (0, "0s"),
        (42, "42s"),
        (59, "59s"),
        (60, "1m 0s"),
        (1511, "25m 11s"),
        (3599, "59m 59s"),
        (3600, "1h 00m"),
        (3900, "1h 05m"),
        (86400, "24h 00m"),
    ] {
        let status = Arc::new(Mutex::new(SurfaceStatus {
            activity: true,
            activity_since: Some(Instant::now() - Duration::from_secs(seconds)),
            ..SurfaceStatus::default()
        }));
        let waiting = Waiting::new(status);
        let indicator = waiting.render_prompt_indicator(PromptEditMode::Emacs);
        assert!(
            indicator.contains(&format!(
                "Working {expected} \x1b[2m· Esc to interrupt\x1b[22m\n"
            )),
            "{seconds}s rendered as {indicator:?}"
        );
    }
}

// Catches: prose after a shell command is printed before that command's receipt
// when earlier prose left an answer block open.
#[test]
fn shell_receipt_precedes_prose_after_an_existing_answer_block() {
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    let (sender, rows) = crossbeam::channel::unbounded();
    let mut output = SurfaceOutput::new(sender, CancelToken::new());
    output.status = Some(status);
    output.write_answer("before command\n").unwrap();
    output
        .write_shell(
            "$ completed command",
            crate::render::Line::plain("$ completed command"),
        )
        .unwrap();
    output.write_answer("after command\n").unwrap();
    let shown = rows.try_iter().collect::<String>();
    let before = shown.find("before command").unwrap();
    let command = shown
        .find("$ completed command")
        .expect("shell receipt must settle before visible prose");
    let after = shown.find("after command").unwrap();
    assert!(
        before < command && command < after,
        "receipt order: {shown}"
    );
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
fn working_line_stays_above_the_composer_after_the_reply_starts() {
    let status = Arc::new(Mutex::new(SurfaceStatus {
        activity: true,
        ..SurfaceStatus::default()
    }));
    let waiting = Waiting::new(Arc::clone(&status));

    assert_eq!(waiting.render_prompt_left().as_ref(), "");
    status
        .lock()
        .expect("surface status lock")
        .reply_started
        .store(true, Ordering::Release);
    let indicator = waiting.render_prompt_indicator(PromptEditMode::Emacs);
    assert!(indicator.contains("Working 0s"));
    assert!(!indicator.contains("Esc cancels"));
    assert!(indicator.ends_with("\n> "));
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

// Catches: expansion waiting in the host input queue until a running turn ends.
#[test]
fn expand_writes_scrollback_without_waiting_for_the_host() {
    let mut editor = Reedline::create();
    let phase = SurfacePhase::new(EditPhase::Idle);
    let status = Arc::new(Mutex::new(SurfaceStatus::with_registry(Arc::new(
        ActionRegistry::final_surface(),
    ))));

    let (sender, rows) = crossbeam::channel::unbounded();
    let output = Arc::new(crate::output::InteractiveOutput::new(SurfaceOutput::new(
        sender,
        CancelToken::new(),
    )));
    let diagnostics: Arc<dyn crate::output::Diagnostics> = output.clone();
    diagnostics.tool_row("compact row", "full output".to_owned());
    rows.try_iter().for_each(drop);
    status.lock().unwrap().diagnostics = Some(Arc::downgrade(&diagnostics));

    let line = apply_surface_effect(
        &mut editor,
        &phase,
        &status,
        SurfaceActionEffect::Expand,
        false,
    );

    assert_eq!(line, None);
    assert_eq!(rows.try_iter().collect::<Vec<_>>(), ["\n", "full output\n"]);
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
        mode.parse_event(key(KeyCode::Char('a'))),
        ReedlineEvent::ExecuteHostCommand(format!("{PERMISSION_DECISION_COMMAND}remember"))
    );
    assert_eq!(
        mode.parse_event(key(KeyCode::Char('d'))),
        ReedlineEvent::ExecuteHostCommand(format!("{PERMISSION_DECISION_COMMAND}details"))
    );
    assert_eq!(
        mode.parse_event(key(KeyCode::Char('r'))),
        ReedlineEvent::None,
        "the retired remember key must not act"
    );
    status.lock().unwrap().permission_modal = false;
    assert_ne!(
        mode.parse_event(key(KeyCode::Char('y'))),
        ReedlineEvent::ExecuteHostCommand(format!("{PERMISSION_DECISION_COMMAND}allow")),
        "a key delivered after modal ownership ended must not approve"
    );
}

#[test]
fn absent_statusline_uses_default_but_prompt_only_is_preserved() {
    use ego_core::config::StatuslineLayer;
    assert_eq!(
        effective_statusline_template(None),
        super::super::status::DEFAULT_TEMPLATE
    );
    assert_eq!(
        effective_statusline_template(Some(&StatuslineLayer {
            providers: None,
            template: Some("{{ prompt }}".into()),
        })),
        "{{ prompt }}"
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
fn help_lists_a_keys_section_with_every_registry_key_at_every_width() {
    // Catches: a hand-written key list that drifts from the registry.
    let registry = ActionRegistry::final_surface().with_binding(
        crate::repl::actions::KeyBinding::new("Ctrl+Q", "quits", "Leave without asking"),
    );
    let keys = registry.keys();
    assert!(keys.len() >= 5, "Esc, Enter, Ctrl+S, Ctrl+C and Option+P");
    for width in [120, 80, 60, 40] {
        let rendered = render_action_help(&registry, width);
        let section = rendered
            .split_once("\nKeys\n")
            .unwrap_or_else(|| panic!("no Keys section at {width}:\n{rendered}"))
            .1;
        let compact = |text: &str| text.split_whitespace().collect::<String>();
        let section = compact(section);
        for key in &keys {
            assert!(
                section.contains(&compact(&key.label))
                    && section.contains(&compact(key.description)),
                "{:?} was lost at {width}:\n{rendered}",
                key.label
            );
        }
    }
}

#[test]
fn banner_hints_list_the_hinted_keys_and_fit_without_dropping_one() {
    let registry = ActionRegistry::final_surface();
    for width in [120, 80, 60, 40] {
        let rendered = render_key_hints(&registry, width);
        assert!(
            rendered
                .lines()
                .all(|line| crate::render::line::cells(line) <= width),
            "banner exceeded {width} cells:\n{rendered}"
        );
        let rows = rendered.lines().map(str::trim).collect::<Vec<_>>();
        for item in [
            "Esc interrupts",
            "Enter queues",
            "Ctrl+Enter steers",
            "Ctrl+S draft",
            "Option+P models",
            "/help",
        ] {
            assert!(
                rows.iter()
                    .any(|row| row.split(" · ").any(|part| part == item)),
                "{item:?} missing or split at {width}:\n{rendered}"
            );
        }
    }
}

#[test]
fn adding_a_registry_key_changes_the_banner_and_the_help_without_editing_either() {
    // Catches: the banner and the help each keeping their own key table.
    let registry = ActionRegistry::final_surface().with_binding(
        crate::repl::actions::KeyBinding::new("Ctrl+Q", "quits", "Leave without asking"),
    );
    assert!(render_key_hints(&registry, 120).contains("Ctrl+Q quits"));
    assert!(render_action_help(&registry, 120).contains("Ctrl+Q  Leave without asking"));
    let plain = ActionRegistry::final_surface();
    assert!(!render_key_hints(&plain, 120).contains("Ctrl+Q"));
    assert!(!render_action_help(&plain, 120).contains("Ctrl+Q"));
}

#[test]
fn reading_a_prebuffered_submission_settles_busy_after_idle() {
    let phase = SurfacePhase::new(EditPhase::Idle);
    let (_inspection_sender, inspections) = crossbeam::channel::unbounded();
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
            inspections,
            inspection_waker: Arc::new(futures::task::AtomicWaker::new()),
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

// Catches: an inspection queued in the finishing turn is answered against a new session.
#[test]
fn pending_inspections_are_answered_before_a_queued_session_switch() {
    let (inspection_sender, inspections) = crossbeam::channel::unbounded();
    let (input_sender, input) = crossbeam::channel::unbounded();
    let (control, _control_receiver) = bounded(CHANNEL_CAPACITY);
    let (_ingress, coordinator) = interactive_control();
    let registry = Arc::new(ActionRegistry::final_surface());
    let seat = Seat {
        kind: SeatKind::Live(Box::new(LiveSeat {
            input,
            inspections,
            inspection_waker: Arc::new(futures::task::AtomicWaker::new()),
            control,
            activated: AtomicBool::new(false),
            break_signal: Arc::new(AtomicBool::new(false)),
            phase: SurfacePhase::new(EditPhase::Idle),
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
    // The active driver can finish with an inspection still waiting. A subsequent
    // /new must not move that already-admitted request to the next session.
    // Repeat because crossbeam randomizes selection when both inboxes are ready.
    for _ in 0..32 {
        let inspection = Line::HostCommand(HostCommand::Action(ActionId::Usage));
        inspection_sender
            .send(HostCommand::Action(ActionId::Usage))
            .unwrap();
        input_sender
            .send(Line::HostCommand(HostCommand::Action(ActionId::New)))
            .unwrap();
        assert_eq!(
            seat.read(),
            inspection,
            "pending /usage must be answered before /new changes the active session"
        );
        assert_eq!(
            seat.read(),
            Line::HostCommand(HostCommand::Action(ActionId::New))
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
        images: super::super::ImageDraft::default(),
        text: "source-first".to_string(),
        cursor: 12,
        origin: InputOrigin::Typed,
    });
    drafts.push_active(ComposerDraft {
        images: super::super::ImageDraft::default(),
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
        images: super::super::ImageDraft::default(),
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

fn modal_after_waiting(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    waited: std::time::Duration,
) {
    let saved = enter_modal(editor, phase, status);
    {
        let mut status = status.lock().unwrap();
        status.modal_since = Some(Instant::now().checked_sub(waited).expect("clock range"));
    }
    leave_modal(editor, phase, status, saved, EditPhase::Busy);
}

// catches: `modal_since` surviving `leave_modal`, so a later modal credits the
// earlier wait to the turn a second time and the counter runs short.
#[test]
fn back_to_back_modals_each_leave_their_own_wait_out_of_the_counter_once() {
    let mut editor = Reedline::create();
    let phase = SurfacePhase::new(EditPhase::Busy);
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    {
        let mut status = status.lock().unwrap();
        status.begin_activity();
        status.activity_since = Some(
            Instant::now()
                .checked_sub(std::time::Duration::from_secs(20))
                .expect("clock range"),
        );
    }
    modal_after_waiting(
        &mut editor,
        &phase,
        &status,
        std::time::Duration::from_secs(3),
    );
    modal_after_waiting(
        &mut editor,
        &phase,
        &status,
        std::time::Duration::from_secs(4),
    );
    let elapsed = status
        .lock()
        .unwrap()
        .activity_since
        .expect("turn still running")
        .elapsed()
        .as_secs();
    assert!(
        (12..=14).contains(&elapsed),
        "20s turn minus 3s and 4s of modal waits is about 13s, got {elapsed}s"
    );
}

// catches: a modal that outlives its turn (cancel or timeout while the
// permission modal is still open) crediting its whole wait to the NEXT turn,
// pushing that turn's start into the future so its counter sticks at 0s.
#[test]
fn a_modal_that_outlived_its_turn_does_not_move_the_next_turn_into_the_future() {
    let mut editor = Reedline::create();
    let phase = SurfacePhase::new(EditPhase::Busy);
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    status.lock().unwrap().begin_activity();
    let saved = enter_modal(&mut editor, &phase, &status);
    {
        let mut status = status.lock().unwrap();
        status.modal_since = Some(
            Instant::now()
                .checked_sub(std::time::Duration::from_secs(30))
                .expect("clock range"),
        );
        // `Seat::idle` for the first turn, then the next turn starts while the
        // modal is still open.
        status.activity = false;
        status.activity_since = None;
        status.begin_activity();
    }
    leave_modal(&mut editor, &phase, &status, saved, EditPhase::Busy);
    let started = status
        .lock()
        .unwrap()
        .activity_since
        .expect("next turn running");
    assert!(
        started <= Instant::now(),
        "a turn cannot have started in the future"
    );
    assert!(
        status.lock().unwrap().modal_waited < std::time::Duration::from_secs(5),
        "only the overlap with the running turn counts as its modal wait"
    );
}

// Catches: a blanket LF-to-CRLF replacement doubling the CR of a row that
// already ends in CRLF, or rewriting bytes inside a control sequence.
#[test]
fn startup_printer_rows_end_in_one_crlf_and_keep_their_bytes() {
    let cases = [
        ("header\n", "header\r\n"),
        ("already\r\n", "already\r\n"),
        ("one\ntwo\n", "one\r\ntwo\r\n"),
        ("\x1b[1mbold\x1b[0m\n", "\x1b[1mbold\x1b[0m\r\n"),
        ("unterminated", "unterminated\r\n"),
        ("", ""),
    ];
    for (message, expected) in cases {
        let mut written = Vec::new();
        write_printer_message(&mut written, message).expect("a Vec accepts writes");
        assert_eq!(String::from_utf8(written).unwrap(), expected, "{message:?}");
    }
}

// Catches: owner-thread receipt waits on the full bounded printer before decision delivery.
#[test]
fn permission_receipt_does_not_wait_for_full_printer_queue() {
    let (sender, receiver) = bounded(1);
    sender.send("held pump row".to_owned()).unwrap();
    let cancel = CancelToken::new();
    let (output, pump) = SurfaceOutput::pumped(sender, cancel, Arc::new(AtomicBool::new(false)));
    let SurfaceSender::Pumped(transcript) = &output.sender else {
        unreachable!()
    };
    let status = Arc::new(Mutex::new(SurfaceStatus {
        transcript: Some(transcript.clone()),
        ..SurfaceStatus::default()
    }));
    let (done, received) = bounded(1);
    let owner = std::thread::spawn(move || {
        question::commit_to_transcript(&status, "Allowed run seq 1 3");
        done.send(()).unwrap();
    });
    let completed = received
        .recv_timeout(std::time::Duration::from_millis(250))
        .is_ok();
    // Always release and join the owner, including RED, so no test thread leaks.
    assert_eq!(receiver.recv().unwrap(), "held pump row");
    assert_eq!(receiver.recv().unwrap(), "Allowed run seq 1 3\n");
    owner.join().unwrap();
    drop(output);
    pump.join().unwrap();
    assert!(
        completed,
        "receipt waited for the printer consumer before decision delivery"
    );
}

// Catches: a disconnected pump silently discards the permission receipt.
#[test]
fn disconnected_receipt_pump_leaves_an_operator_notice() {
    let (sender, receiver) = crossbeam::channel::unbounded();
    drop(receiver);
    let status = Arc::new(Mutex::new(SurfaceStatus {
        transcript: Some(sender),
        ..SurfaceStatus::default()
    }));
    question::commit_to_transcript(&status, "Denied run seq 1 3");
    assert_eq!(
        Waiting::new(status)
            .render_prompt_indicator(PromptEditMode::Emacs)
            .as_ref(),
        "Permission receipt could not reach the output pump | > "
    );
}

// Catches: progress using the startup width after a terminal resize.
#[test]
fn working_row_stays_within_live_terminal_width_after_resize() {
    let mut initial = SurfaceStatus {
        activity: true,
        activity_output_chars: 123_456,
        thinking_since: Some(Instant::now() - Duration::from_secs(12)),
        width: 120,
        ..SurfaceStatus::default()
    };
    initial.permission_capabilities.width = 120;
    let status = Arc::new(Mutex::new(initial));
    let mut mode = SurfaceEditMode::with_status(
        Emacs::default(),
        SurfacePhase::new(EditPhase::Idle),
        Arc::clone(&status),
    );
    let waiting = Waiting::new(status);
    for width in [20_u16, 40, 80, 120, 1] {
        assert!(matches!(
            mode.parse_event(ReedlineRawEvent::try_from(Event::Resize(width, 24)).unwrap()),
            ReedlineEvent::Resize(w, 24) if w == width
        ));
        let indicator = waiting.render_prompt_indicator(PromptEditMode::Emacs);
        let row = indicator.lines().next().unwrap();
        let visible = row.replace("\x1b[2m", "").replace("\x1b[22m", "");
        assert!(
            crate::render::line::cells(&visible) <= usize::from(width).saturating_sub(1),
            "{width}: {row:?}"
        );
        assert!(indicator.ends_with("\n> "), "{indicator:?}");
    }
}

// Catches: a long retry cause wraps onto the composer, or an elapsed delay underflows.
#[test]
fn retry_row_is_bounded_and_expired_countdown_saturates() {
    for width in [20, 80, 120] {
        let mut status = SurfaceStatus {
            retry: Some((
                format!(
                    "{} — connection problem, retrying in 0.1s (attempt 3/3)",
                    "日".repeat(200)
                ),
                Instant::now() - Duration::from_secs(10),
                100,
            )),
            ..SurfaceStatus::default()
        };
        status.permission_capabilities.width = width;
        let waiting = Waiting::new(Arc::new(Mutex::new(status)));
        let indicator = waiting.render_prompt_indicator(PromptEditMode::Emacs);
        let row = indicator.lines().next().unwrap();
        assert_eq!(row.lines().count(), 1);
        assert!(unicode_width::UnicodeWidthStr::width(row.trim_end()) < width);
    }
    let status = SurfaceStatus {
        retry: Some((
            "overload — connection problem, retrying in 0.1s (attempt 3/3)".into(),
            Instant::now() - Duration::from_secs(10),
            100,
        )),
        ..SurfaceStatus::default()
    };
    let waiting = Waiting::new(Arc::new(Mutex::new(status)));
    assert!(
        waiting
            .render_prompt_indicator(PromptEditMode::Emacs)
            .contains("retrying in 0.0s")
    );
}

#[test]
fn persistent_instruction_actions_487_route_menus_and_refuse_busy_ingress() {
    // Catches: menu actions lack an owner or busy slash input mutates persistent state.
    let pending = Arc::new(Mutex::new(None));
    let mut sink = SurfaceActionSink {
        pending: Arc::clone(&pending),
    };
    let mut editor = Reedline::create();
    let phase = SurfacePhase::new(EditPhase::Idle);
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    for (id, effect, dispatch, expected) in [
        (
            ActionId::Memory,
            SurfaceActionEffect::Memory,
            Dispatch::Memory(String::new()),
            HostCommand::Memory(String::new()),
        ),
        (
            ActionId::Init,
            SurfaceActionEffect::Init,
            Dispatch::Init(String::new()),
            HostCommand::Init(String::new()),
        ),
    ] {
        assert_eq!(
            sink.invoke(LocalAction { id, busy: false }),
            LocalActionOutcome::Completed
        );
        assert_eq!(
            pending
                .lock()
                .unwrap()
                .take()
                .map(|value| std::mem::discriminant(&value)),
            Some(std::mem::discriminant(&effect))
        );
        assert!(super::action_execution::busy_dispatch_refusal(&dispatch).is_some());
        assert!(
            matches!(apply_surface_effect(&mut editor, &phase, &status, effect, false), Some(Line::HostCommand(command)) if command == expected)
        );
    }
}
/// Catches: discovery descriptions omit the grammar of argument-bearing commands.
#[test]
fn slash_help_shows_name_goal_and_mcp_grammar() {
    let registry = ActionRegistry::final_surface();
    let help = render_action_help(&registry, 120);
    for grammar in [
        "/name [show|set TEXT|clear]",
        "/goal [show|set TEXT|achieve|block REASON]",
        "/mcp [toggle NAME | add NAME --http URL | add NAME --stdio -- COMMAND ...]",
    ] {
        assert!(help.contains(grammar), "missing {grammar}: {help}");
        assert!(
            registry
                .iter()
                .any(|action| action.description_text().contains(grammar)),
            "menu must use the same grammar: {grammar}"
        );
    }
}

/// Catches: alias spellings cannot be discovered without accidentally submitting a prompt.
#[test]
fn slash_aliases_complete_without_changing_unknown_prompt_policy() {
    let registry = Arc::new(ActionRegistry::final_surface());
    let mut completer = ActionCompleter { registry };
    for (prefix, alias) in [
        ("/resu", "/resume"),
        ("/cont", "/continue"),
        ("/hist", "/history"),
    ] {
        let result = completer.complete(prefix, prefix.len());
        assert!(result.suggestions().iter().any(|item| item.value == alias));
    }
}

fn vim_surface(phase: SurfacePhase, status: SharedStatus) -> SurfaceEditMode {
    SurfaceEditMode::with_editor(composer_editor(true), phase, status)
}

fn keyboard_event(code: KeyCode, modifiers: KeyModifiers) -> ReedlineRawEvent {
    ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(code, modifiers))).unwrap()
}

// Catches: the wrapper never enters Vim normal mode, or does not return to insertion.
#[test]
fn vim_composer_transitions_between_real_normal_and_insert_editing() {
    use reedline::PromptViMode;
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    let mut mode = vim_surface(SurfacePhase::new(EditPhase::Idle), status);
    assert_eq!(mode.edit_mode(), PromptEditMode::Vi(PromptViMode::Insert));
    assert_eq!(
        mode.parse_event(keyboard_event(KeyCode::Char('h'), KeyModifiers::NONE)),
        ReedlineEvent::Edit(vec![EditCommand::InsertChar('h')])
    );
    mode.parse_event(keyboard_event(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(mode.edit_mode(), PromptEditMode::Vi(PromptViMode::Normal));
    assert_eq!(
        mode.parse_event(keyboard_event(KeyCode::Char('h'), KeyModifiers::NONE)),
        ReedlineEvent::Multiple(vec![ReedlineEvent::UntilFound(vec![
            ReedlineEvent::MenuLeft,
            ReedlineEvent::Edit(vec![EditCommand::MoveLeft { select: false }]),
        ])])
    );
    mode.parse_event(keyboard_event(KeyCode::Char('i'), KeyModifiers::NONE));
    assert_eq!(mode.edit_mode(), PromptEditMode::Vi(PromptViMode::Insert));
    mode.parse_event(keyboard_event(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(mode.edit_mode(), PromptEditMode::Vi(PromptViMode::Normal));
    let waiting = Waiting::new(Arc::new(Mutex::new(SurfaceStatus::default())));
    assert!(
        waiting
            .render_prompt_indicator(mode.edit_mode())
            .contains("[NORMAL]")
    );
}

// Catches: paste is interpreted as Vim commands or submits its first line; IME text is eaten.
#[test]
fn vim_composer_preserves_unicode_ime_paste_and_explicit_newlines() {
    use reedline::PromptViMode;
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    let mut mode = vim_surface(SurfacePhase::new(EditPhase::Idle), Arc::clone(&status));
    for (character, modifiers) in [
        ('界', KeyModifiers::NONE),
        ('é', KeyModifiers::CONTROL | KeyModifiers::ALT),
    ] {
        assert_eq!(
            mode.parse_event(keyboard_event(KeyCode::Char(character), modifiers)),
            ReedlineEvent::Edit(vec![EditCommand::InsertChar(character)])
        );
    }
    for normal in [false, true] {
        if normal {
            mode.parse_event(keyboard_event(KeyCode::Esc, KeyModifiers::NONE));
        }
        assert_eq!(
            mode.parse_event(
                ReedlineRawEvent::try_from(Event::Paste("i界\r\n/help\rbye".into())).unwrap()
            ),
            ReedlineEvent::Edit(vec![EditCommand::InsertString("i界\n/help\nbye".into())])
        );
        assert_eq!(status.lock().unwrap().origin, InputOrigin::BracketedPaste);
        assert_eq!(
            mode.parse_event(keyboard_event(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline])
        );
        assert_eq!(
            mode.parse_event(keyboard_event(KeyCode::Enter, KeyModifiers::CONTROL)),
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline])
        );
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            assert_eq!(
                mode.parse_event(keyboard_event(KeyCode::Enter, modifiers)),
                ReedlineEvent::Edit(vec![EditCommand::InsertNewline])
            );
        }
        assert_eq!(
            mode.edit_mode(),
            PromptEditMode::Vi(if normal {
                PromptViMode::Normal
            } else {
                PromptViMode::Insert
            })
        );
    }
}

// Catches: Vim consumes queue/steer/interrupt/draft keys while a turn runs,
// or a staged image steals busy Escape for image removal instead of interruption.
#[test]
fn vim_composer_keeps_busy_host_controls_in_both_modes() {
    let phase = SurfacePhase::new(EditPhase::Idle);
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    let mut mode = vim_surface(phase.clone(), Arc::clone(&status));
    for normal in [false, true] {
        phase.set(EditPhase::Idle);
        status.lock().unwrap().image_chips.clear();
        if normal {
            mode.parse_event(keyboard_event(KeyCode::Esc, KeyModifiers::NONE));
        }
        phase.set(EditPhase::Busy);
        status.lock().unwrap().image_chips = vec!["clipboard 1".into()];
        let before = mode.edit_mode();
        for (code, modifiers, command) in [
            (KeyCode::Enter, KeyModifiers::NONE, BUSY_SUBMIT_COMMAND),
            (KeyCode::Enter, KeyModifiers::CONTROL, BUSY_STEER_COMMAND),
            (KeyCode::Esc, KeyModifiers::NONE, INTERRUPT_COMMAND),
            (KeyCode::Char('s'), KeyModifiers::CONTROL, DRAFT_COMMAND),
            (
                KeyCode::Char('b'),
                KeyModifiers::CONTROL,
                BACKGROUND_COMMAND,
            ),
        ] {
            assert_eq!(
                mode.parse_event(keyboard_event(code, modifiers)),
                ReedlineEvent::ExecuteHostCommand(command.into())
            );
            assert_eq!(mode.edit_mode(), before);
        }
        status.lock().unwrap().queue.queued = 1;
        let event = mode.parse_event(keyboard_event(KeyCode::Up, KeyModifiers::NONE));
        let ReedlineEvent::UntilFound(events) = event else {
            panic!("missing queue recall")
        };
        assert_eq!(
            events[0],
            ReedlineEvent::ExecuteHostCommandIfEmpty(BUSY_RECALL_COMMAND.into())
        );
        assert_eq!(
            mode.parse_event(keyboard_event(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline])
        );
    }
}

// Catches: normal-mode editing swallows approval keys or leaks into a modal text form.
#[test]
fn vim_composer_never_takes_permission_or_modal_text_ownership() {
    let phase = SurfacePhase::new(EditPhase::Idle);
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    let mut mode = vim_surface(phase.clone(), Arc::clone(&status));
    mode.parse_event(keyboard_event(KeyCode::Esc, KeyModifiers::NONE));
    let before = mode.edit_mode();
    phase.set(EditPhase::Modal);
    status.lock().unwrap().permission_modal = true;
    assert_eq!(mode.edit_mode(), PromptEditMode::Emacs);
    assert_eq!(
        mode.parse_event(keyboard_event(KeyCode::Char('y'), KeyModifiers::NONE)),
        ReedlineEvent::ExecuteHostCommand(format!("{PERMISSION_DECISION_COMMAND}allow"))
    );
    assert_eq!(
        mode.parse_event(keyboard_event(KeyCode::Esc, KeyModifiers::NONE)),
        ReedlineEvent::ExecuteHostCommand(MODAL_ESCAPE_COMMAND.into())
    );
    assert_eq!(
        mode.parse_event(ReedlineRawEvent::try_from(Event::Paste("y".into())).unwrap()),
        ReedlineEvent::None
    );
    status.lock().unwrap().permission_modal = false;
    assert_eq!(
        mode.parse_event(keyboard_event(KeyCode::Char('h'), KeyModifiers::NONE)),
        ReedlineEvent::Edit(vec![EditCommand::InsertChar('h')])
    );
    phase.set(EditPhase::Idle);
    assert_eq!(
        mode.edit_mode(),
        before,
        "a modal must not change the composer mode"
    );
}

// Catches: action help retains the old shortcut after runtime dispatch was changed.
#[test]
fn configured_shortcuts_reach_the_surface_and_both_help_projections() {
    let registry = Arc::new(
        ActionRegistry::final_surface()
            .with_keybindings(&std::collections::BTreeMap::from([(
                "model".into(),
                vec!["Ctrl+P".into()],
            )]))
            .unwrap(),
    );
    let status = Arc::new(Mutex::new(SurfaceStatus::with_registry(Arc::clone(
        &registry,
    ))));
    let mut mode = vim_surface(SurfacePhase::new(EditPhase::Idle), status);
    assert_eq!(
        mode.parse_event(keyboard_event(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        ReedlineEvent::ExecuteHostCommand(format!("{ACTION_COMMAND}model"))
    );
    for help in [
        render_action_help(&registry, 120),
        render_key_hints(&registry, 120),
    ] {
        assert!(help.contains("Ctrl+P"), "{help}");
        assert!(!help.contains("Option+P"), "{help}");
    }
}

// Catches: a submitted prompt longer than the preview limit floods the transcript.
#[test]
fn long_prompt_echo_is_collapsed() {
    let waiting = Waiting::new(Arc::new(Mutex::new(SurfaceStatus::default())));
    let prompt = (1..=20)
        .map(|n| format!("brief line {n}\n"))
        .collect::<String>();
    let receipt = waiting
        .render_prompt_submission(&prompt, 120)
        .expect("long submissions need a compact receipt");
    assert_eq!(
        receipt,
        "> brief line 1\n… brief line 2\n… brief line 3\n  … 17 more lines · Option+O or /expand"
    );
}

// Catches: collapsed input is not reachable through the tool expansion action.
#[test]
fn collapsed_prompt_and_tool_output_share_expansion_history() {
    use crate::output::{Diagnostics, InteractiveOutput};
    let status = Arc::new(Mutex::new(SurfaceStatus::default()));
    let waiting = Waiting::new(Arc::clone(&status));
    let (sender, rows) = crossbeam::channel::unbounded();
    let mut surface = SurfaceOutput::new(sender, CancelToken::new());
    surface.details = Arc::clone(&status.lock().unwrap().details);
    let output = InteractiveOutput::new(surface);
    output.tool_row("old tool", "old detail".into());
    let _ = rows.try_iter().collect::<Vec<_>>();
    let prompt = "one\ntwo\nthree\nfour";
    assert!(waiting.render_prompt_submission(prompt, 80).is_some());
    output.expand_last_tool();
    assert!(
        rows.try_iter()
            .collect::<String>()
            .contains("> one\n… two\n… three\n… four")
    );
    // A short submission, including /expand itself, must not replace the detail.
    assert_eq!(
        waiting.render_prompt_submission("/expand", 80).as_deref(),
        Some("> /expand")
    );
    output.expand_last_tool();
    assert_eq!(
        rows.try_iter().collect::<String>(),
        "\n> one\n… two\n… three\n… four\n"
    );
    output.tool_row("new tool", "new detail".into());
    let _ = rows.try_iter().collect::<Vec<_>>();
    output.expand_last_tool();
    assert!(rows.try_iter().collect::<String>().contains("new detail"));
}

// Catches an invalid template repeating its diagnostic on each background repaint.
#[test]
fn template_diagnostic_is_emitted_only_once_per_configuration() {
    let snapshot = SurfaceSnapshot::from_state(
        SessionId::new(),
        &SessionState::default(),
        SelectionFacts::new(
            ego_core::config::ModelRef("provider/model".into()),
            ego_core::generator::types::ThinkingLevel::Medium,
            Mode::Yolo,
        ),
        SeatFacts::new(QueueStatus::default(), 0, "/workspace", Trust::Trusted),
        None,
    );
    let status = Arc::new(Mutex::new(SurfaceStatus {
        native_status: true,
        width: 120,
        snapshot: Some(snapshot),
        template: super::super::status::StatusTemplate::new("{{ unknown }}\n{{ prompt }}".into()),
        ..SurfaceStatus::default()
    }));
    let waiting = Waiting::new(Arc::clone(&status));
    for _ in 0..3 {
        repaint_status(&status);
        let output = format!(
            "{}diagnostic-probe",
            waiting.render_prompt_indicator(PromptEditMode::Emacs)
        );
        let mut terminal = vt100::Parser::new(12, 120, 0);
        terminal.process(output.replace('\n', "\r\n").as_bytes());
        let visible = terminal.screen().contents();
        assert_eq!(visible.matches("statusline:").count(), 1, "{visible}");
        assert!(visible.contains("(line 1)"), "{visible}");
        assert!(visible.ends_with("> diagnostic-probe"), "{visible}");
    }
}

// Catches: a session switch or draft restore keeps a global image list and
// attaches the source's images to a different session's text.
#[test]
fn session_switch_restores_text_and_attachments_as_one_draft() {
    use ego_core::generator::types::{ImageMediaType, ImageRef};
    use ego_core::store::blob::{BlobId, BlobRef};
    let bytes = b"published session image";
    let reference = ImageRef::new(
        BlobRef::new(
            BlobId::parse(ego_core::digest::sha256_hex(bytes)).unwrap(),
            bytes.len() as u64,
        ),
        ImageMediaType::Png,
        1,
        1,
    )
    .unwrap();
    let images = super::super::ImageDraft::recalled(&[reference]);
    let source = SessionId::new();
    let target = SessionId::new();
    let phase = SurfacePhase::new(EditPhase::Idle);
    let status = Arc::new(Mutex::new(SurfaceStatus {
        images,
        ..SurfaceStatus::default()
    }));
    let mut editor = Reedline::create();
    editor.run_edit_commands(&[EditCommand::InsertString("source text".into())]);
    let asked = AtomicBool::new(false);
    let (ingress, _control) = interactive_control();
    let mut drafts = DraftStash::new(source);
    let (_control, controls) = crossbeam::channel::unbounded();
    for session in [target, source] {
        let (reply, answer) = bounded(1);
        let mut surface = ModalSurface {
            phase: &phase,
            status: &status,
            asked_for_image: &asked,
            ingress: &ingress,
            drafts: &mut drafts,
        };
        assert!(answer_control(
            &mut editor,
            &phase,
            &mut surface,
            &controls,
            &mut None,
            Control::ActivateSession {
                target: session,
                reply
            }
        ));
        assert!(answer.recv().unwrap());
        if session == target {
            assert!(editor.current_buffer_contents().is_empty());
            assert!(status.lock().unwrap().images.is_empty());
            // Clearing the target's active attachment handle must not clear
            // the saved source handle.
            status.lock().unwrap().images.clear();
        }
    }
    assert_eq!(editor.current_buffer_contents(), "source text");
    assert_eq!(status.lock().unwrap().images.labels(), ["recalled 1"]);
    assert_eq!(drafts.active_count(), 0);
}

// Catches: Shift+Tab edits/submits a draft or changes authority behind an open modal.
#[test]
fn shift_tab_is_a_local_command_in_idle_and_busy_but_not_modals() {
    for phase in [EditPhase::Idle, EditPhase::Busy, EditPhase::Modal] {
        for vim in [false, true] {
            let state = SurfacePhase::new(phase);
            let status = Arc::new(Mutex::new(SurfaceStatus::default()));
            let mut mode = if vim {
                vim_surface(state, status)
            } else {
                SurfaceEditMode::with_status(emacs_composer(), state, status)
            };
            assert_eq!(
                mode.parse_event(keyboard_event(KeyCode::BackTab, KeyModifiers::SHIFT)),
                if phase == EditPhase::Modal {
                    ReedlineEvent::None
                } else {
                    ReedlineEvent::ExecuteHostCommand(MODE_CYCLE_COMMAND.into())
                }
            );
        }
    }
}
