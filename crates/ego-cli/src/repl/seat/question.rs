use super::edit_mode::PICKER_KEYS;
use super::permission_modal::{compact_prompt, details_prompt, settled_receipt};
use super::*;
use crate::render::osc::State;

pub(super) fn enter_modal(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
) -> SavedComposer {
    let saved = SavedComposer {
        text: editor.current_buffer_contents().to_string(),
        cursor: editor.current_insertion_point(),
        origin: status.lock().expect("surface status lock").origin,
    };
    editor.run_edit_commands(&[EditCommand::Clear]);
    phase.set(EditPhase::Modal);
    {
        let mut status = status.lock().expect("surface status lock");
        status.notice = None;
        status.modal_since = Some(Instant::now());
        if let Some(snapshot) = status.snapshot.as_mut() {
            snapshot.set_modal(true);
        }
    }
    repaint_status(status);
    saved
}

pub(super) fn leave_modal(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    saved: SavedComposer,
    after: EditPhase,
) {
    editor.run_edit_commands(&[
        EditCommand::Clear,
        EditCommand::InsertString(saved.text),
        EditCommand::MoveToPosition {
            position: saved.cursor,
            select: false,
        },
    ]);
    {
        let mut status = status.lock().expect("surface status lock");
        let status = &mut *status;
        status.origin = saved.origin;
        status.notice = None;
        // Keys of an unfinished burst belong to the modal that is closing.
        status.picker_keys.clear();
        // The turn's elapsed time leaves out the wait for the person, and
        // only the part of the wait that fell inside the running turn: a modal
        // that outlived its turn must not push the next one into the future.
        if let Some(since) = status.modal_since.take()
            && let Some(started) = status.activity_since.as_mut()
        {
            let waited = since.max(*started).elapsed();
            *started += waited;
            status.modal_waited += waited;
        }
        if let Some(snapshot) = status.snapshot.as_mut() {
            snapshot.set_modal(false);
        }
    }
    phase.set(after);
    repaint_status(status);
}

fn confirm_compaction_drafts(
    editor: &mut Reedline,
    surface: &mut ModalSurface<'_>,
    source: SessionId,
) -> bool {
    if surface.drafts.active() != source {
        false
    } else {
        let drafts = surface.drafts.active_count();
        if drafts == 0 {
            true
        } else {
            let prompt = Question::new(
                format!(
                    "Compact session {source} into a new clean durable session while retaining {drafts} source-owned drafts? [y/N] "
                ),
                Arc::clone(surface.status),
            );
            loop {
                match modal_line(editor, surface, &prompt) {
                    Line::Submitted(answer) => match parse_trust(&answer) {
                        Some(Trust::Trusted) => break true,
                        Some(Trust::Untrusted) => break false,
                        None => continue,
                    },
                    Line::Interrupted
                    | Line::Blank
                    | Line::Ended
                    | Line::ExternalBreak(_)
                    | Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                        break false;
                    }
                    Line::Refused(_) | Line::HostCommand(_) => continue,
                }
            }
        }
    }
}

pub(super) fn answer_control(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    surface: &mut ModalSurface<'_>,
    controls: &Receiver<Control>,
    pending_control: &mut Option<Control>,
    control: Control,
) -> bool {
    let control = match control {
        Control::InsertPasteText { text } => {
            editor.run_edit_commands(&[EditCommand::InsertString(text)]);
            surface.status.lock().expect("surface status lock").origin =
                InputOrigin::BracketedPaste;
            repaint_status(surface.status);
            return true;
        }
        Control::ActivateSession { target, reply } => {
            surface.drafts.activate(target);
            {
                let mut current = surface.status.lock().expect("surface status lock");
                if let Some(base) = current.status_base.as_mut() {
                    base.session = target;
                }
                current.drafts = surface.drafts.active_count();
                let draft_count = current.drafts;
                if let Some(snapshot) = current.snapshot.as_mut() {
                    snapshot.set_drafts(draft_count);
                }
                current.notice = None;
            }
            repaint_status(surface.status);
            let _ = reply.send(true);
            return true;
        }
        control => control,
    };
    let control = match control {
        Control::SessionPicker { sessions, reply } => {
            let selected =
                super::session_picker::run_session_picker(editor, phase, surface.status, sessions);
            let _ = reply.send(selected);
            return true;
        }
        control => control,
    };
    let control = match control {
        Control::StartupModel {
            picker,
            notice,
            reply,
        } => {
            let result = run_model_picker(editor, phase, surface.status, picker, false, notice);
            let outcome = match result {
                Some(Line::HostCommand(HostCommand::ModelSwitch(request))) => {
                    PickerOutcome::ModelSwitch(request)
                }
                _ => PickerOutcome::Cancelled,
            };
            let _ = reply.send(outcome);
            return true;
        }
        control => control,
    };
    let control = match control {
        Control::SelectSetup { request, reply } => {
            let outcome = if phase.get() == EditPhase::Busy {
                QuestionOutcome::Unavailable
            } else {
                let after = phase.get();
                let saved = enter_modal(editor, phase, surface.status);
                let outcome = answer_question(editor, surface, &request);
                leave_modal(editor, phase, surface.status, saved, after);
                outcome
            };
            let _ = reply.send(outcome);
            return true;
        }
        Control::ConfirmSetup { text, reply } => {
            let confirmation = if phase.get() == EditPhase::Busy {
                Confirmation::Declined
            } else {
                confirm_surface_action(editor, phase, surface.status, &text, false)
            };
            let _ = reply.send(confirmation);
            return true;
        }
        Control::ConfirmRemember { reply } => {
            let busy = phase.get() == EditPhase::Busy;
            let confirmation = confirm_surface_action(
                editor,
                phase,
                surface.status,
                "Apply these remembered permission rules? [y/N] ",
                busy,
            );
            let _ = reply.send(confirmation);
            return true;
        }
        control => control,
    };
    if matches!(&control, Control::Activate { .. }) {
        return true;
    }
    let saved = enter_modal(editor, phase, surface.status);

    enum Reply {
        Compaction(Sender<bool>, bool),
        Trust(Sender<Trust>, Trust),
        WindowsUnconfined(Sender<bool>, bool),
        Permission(
            oneshot::Sender<PermissionDecision>,
            PermissionDecision,
            String,
        ),
        Question(oneshot::Sender<QuestionOutcome>, QuestionOutcome),
    }

    let (reply, after, mut keep_running) = match control {
        Control::ConfirmCompaction { source, reply } => {
            let confirmed = confirm_compaction_drafts(editor, surface, source);
            (Reply::Compaction(reply, confirmed), EditPhase::Idle, true)
        }
        Control::Trust {
            workspace,
            context,
            reply,
        } => {
            let prompt = Question::new(
                format!("{context}\nTrust project files in {workspace}? [y/N] "),
                Arc::clone(surface.status),
            );
            let trust = loop {
                match modal_line(editor, surface, &prompt) {
                    Line::Submitted(answer) => match parse_trust(&answer) {
                        Some(trust) => break trust,
                        None => continue,
                    },
                    Line::Blank | Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => {
                        break Trust::Untrusted;
                    }
                    Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                        break Trust::Untrusted;
                    }
                    Line::Refused(_) | Line::HostCommand(_) => continue,
                }
            };
            (Reply::Trust(reply, trust), EditPhase::Idle, true)
        }
        Control::WindowsUnconfined { workspace, reply } => {
            let prompt = Question::new(
                format!(
                    "Windows execution is unconfined in {workspace}; filesystem and network \
                     isolation are not enforced. Continue? [y/N] "
                ),
                Arc::clone(surface.status),
            );
            let accepted = loop {
                match modal_line(editor, surface, &prompt) {
                    Line::Submitted(answer) => match parse_trust(&answer) {
                        Some(Trust::Trusted) => break true,
                        Some(Trust::Untrusted) => break false,
                        None => continue,
                    },
                    Line::Blank | Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => {
                        break false;
                    }
                    Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                        break false;
                    }
                    Line::Refused(_) | Line::HostCommand(_) => continue,
                }
            };
            (
                Reply::WindowsUnconfined(reply, accepted),
                EditPhase::Idle,
                true,
            )
        }
        Control::Permission { request, reply } => {
            surface
                .status
                .lock()
                .expect("surface status lock")
                .permission_modal = true;
            let capabilities = {
                let status = surface.status.lock().expect("surface status lock");
                crate::render::Capabilities {
                    width: status.width.max(1),
                    ..status.permission_capabilities
                }
            };
            let mut prompt = Question::new(
                compact_prompt(&request, capabilities.width),
                Arc::clone(surface.status),
            );
            tell_host(surface.status, |osc, quiet| {
                osc.awaiting(request.tool(), !quiet)
            });
            let mut notice = None;
            let decision = loop {
                match permission_line(editor, surface, &prompt) {
                    // The keys: `n` asks what to do instead; typed words below
                    // are the plain prompt and stay a bare refusal.
                    Line::HostCommand(HostCommand::PermissionDecision(decision)) => {
                        match decision.as_str() {
                            "details" => {
                                prompt = Question::new(
                                    details_prompt(&request, capabilities),
                                    Arc::clone(surface.status),
                                );
                            }
                            "deny" => break decline(editor, surface),
                            _ => match parse_permission(&decision) {
                                Some(word) => {
                                    break settle_permission(
                                        editor,
                                        surface,
                                        &request,
                                        word,
                                        &mut notice,
                                    );
                                }
                                None => continue,
                            },
                        }
                    }
                    Line::Submitted(answer) => match parse_permission(&answer) {
                        Some(word) => {
                            break settle_permission(editor, surface, &request, word, &mut notice);
                        }
                        None => continue,
                    },
                    Line::Ended | Line::ExternalBreak(_) => {
                        break PermissionDecision::Deny;
                    }
                    Line::Interrupted
                    | Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                        notice = Some(crate::repl::INTERRUPTED_RECEIPT.to_owned());
                        break PermissionDecision::Deny;
                    }
                    Line::Blank | Line::Refused(_) | Line::HostCommand(_) => continue,
                }
            };
            tell_host(surface.status, |osc, _| osc.state(State::Busy));
            let receipt = settled_receipt(&request, &decision, notice.as_deref());
            (
                Reply::Permission(reply, decision, receipt),
                EditPhase::Busy,
                true,
            )
        }
        Control::Question {
            id: _id,
            request,
            reply,
        } => {
            tell_host(surface.status, |osc, _| osc.state(State::Awaiting));
            let outcome = answer_question(editor, surface, &request);
            tell_host(surface.status, |osc, _| osc.state(State::Busy));
            (Reply::Question(reply, outcome), EditPhase::Busy, true)
        }
        Control::Shutdown => return false,
        Control::InsertPasteText { .. } => {
            unreachable!("paste text is inserted before modal entry")
        }
        Control::ActivateSession { .. } => unreachable!("activation is handled before modal entry"),
        Control::Activate { .. } => unreachable!("activation is handled before modal entry"),
        Control::SelectSetup { .. } | Control::ConfirmSetup { .. } => {
            unreachable!("setup confirmation is handled before modal entry")
        }
        Control::ConfirmRemember { .. } => {
            unreachable!("remember confirmation is handled before modal entry")
        }
        Control::SessionPicker { .. } => {
            unreachable!("session selection is handled before modal entry")
        }
        Control::StartupModel { .. } => {
            unreachable!("startup model selection is handled before modal entry")
        }
    };
    match controls.try_recv() {
        Ok(Control::Shutdown) => keep_running = false,
        Ok(next) => *pending_control = Some(next),
        Err(crossbeam::channel::TryRecvError::Empty) => {}
        Err(crossbeam::channel::TryRecvError::Disconnected) => keep_running = false,
    }

    leave_modal(editor, phase, surface.status, saved, after);
    surface
        .status
        .lock()
        .expect("surface status lock")
        .permission_modal = false;
    match reply {
        Reply::Compaction(sender, confirmed) => {
            let _ = sender.send(confirmed);
        }
        Reply::Trust(sender, trust) => {
            let _ = sender.send(trust);
        }
        Reply::WindowsUnconfined(sender, accepted) => {
            let _ = sender.send(accepted);
        }
        Reply::Permission(sender, decision, receipt) => {
            #[cfg(test)]
            hold_permission_printer();
            commit_to_transcript(surface.status, &receipt);
            let _ = sender.send(decision);
            #[cfg(test)]
            await_permission_printer_release();
        }
        Reply::Question(sender, outcome) => {
            let _ = sender.send(outcome);
        }
    }
    keep_running
}

fn answer_question(
    editor: &mut Reedline,
    surface: &mut ModalSurface<'_>,
    request: &QuestionRequest,
) -> QuestionOutcome {
    if let QuestionMode::Round(round) = &request.mode {
        if request.validate().is_err() {
            return QuestionOutcome::Unavailable;
        }
        if let Some((key, member)) = round.next_member() {
            let answer = match &member.kind {
                ego_core::question::ElicitationKind::Form { schema } => {
                    match answer_question(editor, surface, &member.form_request(schema)) {
                        QuestionOutcome::Answered(QuestionAnswer::Form(answer)) => {
                            serde_json::Value::Object(answer)
                        }
                        outcome => return outcome,
                    }
                }
                ego_core::question::ElicitationKind::Url { url, .. } => {
                    let prompt = Question::new(
                        format!(
                            "{}\n{}\nDestination: {}\nOpen this page? Credentials stay on the page, never in chat. [y/N] ",
                            request.prompt, member.prompt, url
                        ),
                        Arc::clone(surface.status),
                    );
                    let consent = match modal_line(editor, surface, &prompt) {
                        Line::Submitted(value) => parse_trust(&value) == Some(Trust::Trusted),
                        Line::HostCommand(HostCommand::ModalEscape) => {
                            return QuestionOutcome::Cancelled;
                        }
                        Line::Ended | Line::ExternalBreak(_) => {
                            return QuestionOutcome::Unavailable;
                        }
                        _ => false,
                    };
                    if !consent {
                        return QuestionOutcome::Declined;
                    }
                    if !launch_elicitation_url(url) {
                        surface.status.lock().expect("surface status lock").notice = Some(
                            "The browser could not be opened; the request remains pending"
                                .to_owned(),
                        );
                        return QuestionOutcome::Unavailable;
                    }
                    let prompt = Question::new(
                        "Complete the action on the page. Never paste a password or code. Type completed, decline, or cancel.\n> ".to_owned(),
                        Arc::clone(surface.status),
                    );
                    loop {
                        match modal_line(editor, surface, &prompt) {
                            Line::Submitted(value) => match value.trim() {
                                "completed" => break serde_json::json!({"completed":true}),
                                "decline" => return QuestionOutcome::Declined,
                                "cancel" => return QuestionOutcome::Cancelled,
                                _ => surface.status.lock().expect("surface status lock").notice = Some("Use completed, decline, or cancel; never provide credentials".to_owned()),
                            },
                            Line::HostCommand(HostCommand::ModalEscape) => return QuestionOutcome::Cancelled,
                            Line::Interrupted | Line::Blank => return QuestionOutcome::Declined,
                            Line::Ended | Line::ExternalBreak(_) => return QuestionOutcome::Unavailable,
                            _ => {},
                        }
                    }
                }
            };
            return QuestionOutcome::MemberCompleted {
                key: key.to_owned(),
                answer,
            };
        }
        let confirm = QuestionRequest::new(
            "Retry the original MCP operation with these completed inputs?",
            QuestionMode::Single,
            vec![QuestionOption::new("retry", "Retry the original operation")],
            false,
        );
        return match answer_question(editor, surface, &confirm) {
            QuestionOutcome::Answered(QuestionAnswer::Single(value)) if value == "retry" => {
                QuestionOutcome::Answered(QuestionAnswer::Round(
                    ego_core::question::ElicitationAnswers {
                        confirmed: true,
                        responses: round.completed.clone(),
                    },
                ))
            }
            outcome => outcome,
        };
    }
    let cursor_selectors = surface
        .status
        .lock()
        .expect("surface status lock")
        .cursor_selectors;
    if cursor_selectors && matches!(request.mode, QuestionMode::Single | QuestionMode::Multiple) {
        return answer_question_selector(editor, surface, request);
    }
    let prompt = Question::new(question_prompt(request), Arc::clone(surface.status));
    loop {
        match modal_line(editor, surface, &prompt) {
            Line::Submitted(answer) => match parse_question_answer(request, &answer) {
                Ok(answer) => return QuestionOutcome::Answered(answer),
                Err(error) => {
                    surface.status.lock().expect("surface status lock").notice =
                        Some(error.to_string());
                }
            },
            Line::HostCommand(HostCommand::ModalEscape) => {
                return QuestionOutcome::Cancelled;
            }
            Line::Interrupted | Line::Blank => return QuestionOutcome::Declined,
            Line::Ended | Line::ExternalBreak(_) => return QuestionOutcome::Unavailable,
            Line::Refused(_) | Line::HostCommand(_) => {}
        }
    }
}

fn answer_question_selector(
    editor: &mut Reedline,
    surface: &mut ModalSurface<'_>,
    request: &QuestionRequest,
) -> QuestionOutcome {
    let kind = match request.mode {
        QuestionMode::Single => SelectorKind::Single,
        QuestionMode::Multiple => SelectorKind::Multiple,
        QuestionMode::Round(_) => unreachable!("rounds present their members separately"),
        QuestionMode::FreeText => unreachable!("free text retains the editor grammar"),
        QuestionMode::Form { .. } => {
            unreachable!("form answers retain the raw JSON editor grammar")
        }
    };
    let selector = Arc::new(Mutex::new(Selector::with_options(
        kind,
        request
            .options
            .iter()
            .map(|option| SelectorOption::new(option.id.clone(), option.label.clone()))
            .chain(offers_other(request).then(|| SelectorOption::new(OTHER_ID, OTHER_LABEL))),
    )));
    surface
        .status
        .lock()
        .expect("surface status lock")
        .question_selector = Some(Arc::clone(&selector));
    repaint_status(surface.status);

    let outcome = loop {
        let prompt = Question::new(
            question_selector_prompt(request, &selector),
            Arc::clone(surface.status),
        );
        let queued = queued_selector_line(surface.status);
        match queued.unwrap_or_else(|| modal_line(editor, surface, &prompt)) {
            Line::HostCommand(HostCommand::QuestionSelector(command)) => {
                let Some(key) = next_selector_key(surface.status, &command) else {
                    continue;
                };
                let mut guard = selector.lock().expect("question selector lock");
                guard.key(key);
                if key == SelectorKey::Escape {
                    break QuestionOutcome::Cancelled;
                }
                if key != SelectorKey::Enter {
                    drop(guard);
                    surface.status.lock().expect("surface status lock").notice = None;
                    repaint_status(surface.status);
                    continue;
                }
                let result = guard.result().cloned();
                drop(guard);
                let wants_other = match &result {
                    Some(SelectorValue::Single(id)) => id == OTHER_ID,
                    Some(SelectorValue::Multiple(ids)) => ids.iter().any(|id| id == OTHER_ID),
                    Some(SelectorValue::FreeText(_)) | None => false,
                };
                let typed = if wants_other {
                    match other_text(editor, surface, &selector) {
                        OtherEntry::Text(text) => Some(text),
                        OtherEntry::Back => continue,
                        OtherEntry::Settled(outcome) => break outcome,
                    }
                } else {
                    None
                };
                let answer = match (result, typed) {
                    (Some(SelectorValue::Single(_)), Some(text)) => QuestionAnswer::single(text),
                    (Some(SelectorValue::Single(id)), None) => QuestionAnswer::single(id),
                    (Some(SelectorValue::Multiple(ids)), typed) => {
                        QuestionAnswer::multiple(ids.into_iter().map(|id| {
                            match (&typed, id == OTHER_ID) {
                                (Some(text), true) => text.clone(),
                                _ => id,
                            }
                        }))
                    }
                    _ => continue,
                };
                match answer.validate_for(request) {
                    Ok(()) => break QuestionOutcome::Answered(answer),
                    Err(error) => {
                        surface.status.lock().expect("surface status lock").notice =
                            Some(error.to_string());
                        repaint_status(surface.status);
                    }
                }
            }
            Line::Interrupted | Line::Blank => break QuestionOutcome::Declined,
            Line::Ended | Line::ExternalBreak(_) => break QuestionOutcome::Unavailable,
            Line::Submitted(_) | Line::Refused(_) | Line::HostCommand(_) => {}
        }
    };
    surface
        .status
        .lock()
        .expect("surface status lock")
        .question_selector = None;
    repaint_status(surface.status);
    outcome
}

/// An option id is never empty (`QuestionRequest::validate` refuses one), so
/// the empty id names the `Other` row without colliding with a real option.
const OTHER_ID: &str = "";
const OTHER_LABEL: &str = "Other";
const OTHER_DESCRIPTION: &str = "Type your own answer";

/// The `Other` row exists only where the schema lets the model accept free
/// text alongside the options.
fn offers_other(request: &QuestionRequest) -> bool {
    request.free_text && matches!(request.mode, QuestionMode::Single | QuestionMode::Multiple)
}

enum OtherEntry {
    Text(String),
    /// A blank line or Esc: the selector is still open.
    Back,
    Settled(QuestionOutcome),
}

/// Reads the free-text answer behind the `Other` row. The selector keys are
/// released while the human types and taken back before returning to it.
fn other_text(
    editor: &mut Reedline,
    surface: &mut ModalSurface<'_>,
    selector: &Arc<Mutex<Selector>>,
) -> OtherEntry {
    {
        let mut status = surface.status.lock().expect("surface status lock");
        status.question_selector = None;
        // Queued keys belong to the selector this entry replaces; replayed
        // after backing out they would act on a stale choice. Keys the editor
        // has not read yet are typed-ahead input for the entry and stay.
        status.picker_keys.clear();
    }
    let prompt = Question::new(format!("{OTHER_LABEL}: "), Arc::clone(surface.status));
    let entry = loop {
        match modal_line(editor, surface, &prompt) {
            Line::Submitted(text) if !text.trim().is_empty() => {
                break OtherEntry::Text(text.trim().to_string());
            }
            Line::Submitted(_) | Line::Blank | Line::HostCommand(HostCommand::ModalEscape) => {
                break OtherEntry::Back;
            }
            Line::Interrupted => break OtherEntry::Settled(QuestionOutcome::Declined),
            Line::Ended | Line::ExternalBreak(_) => {
                break OtherEntry::Settled(QuestionOutcome::Unavailable);
            }
            Line::Refused(_) | Line::HostCommand(_) => {}
        }
    };
    surface
        .status
        .lock()
        .expect("surface status lock")
        .question_selector = Some(Arc::clone(selector));
    repaint_status(surface.status);
    entry
}

fn question_selector_key(command: &str) -> Option<SelectorKey> {
    match command {
        "up" => Some(SelectorKey::Up),
        "down" => Some(SelectorKey::Down),
        "space" => Some(SelectorKey::Space),
        "enter" => Some(SelectorKey::Enter),
        "escape" => Some(SelectorKey::Escape),
        number => number
            .strip_prefix("number:")?
            .parse()
            .ok()
            .map(SelectorKey::Number),
    }
}

/// The selector line a burst of keys already supplied, if one is waiting.
///
/// The editor parses a whole burst before running its first command and drops
/// the rest, so the edit mode queues every selector key in
/// `SurfaceStatus::picker_keys` — the queue the model picker uses — and the
/// loop applies them one at a time through this and [`next_selector_key`].
pub(super) fn queued_selector_line(status: &SharedStatus) -> Option<Line> {
    let waiting = !status
        .lock()
        .expect("surface status lock")
        .picker_keys
        .is_empty();
    waiting.then(|| Line::HostCommand(HostCommand::QuestionSelector(PICKER_KEYS.to_owned())))
}

/// The oldest queued selector key; `command` is only the wake-up that carried it.
pub(super) fn next_selector_key(status: &SharedStatus, _command: &str) -> Option<SelectorKey> {
    let mut status = status.lock().expect("surface status lock");
    if status.picker_keys.is_empty() {
        return None;
    }
    question_selector_key(&status.picker_keys.remove(0))
}

pub(super) fn question_selector_prompt(
    request: &QuestionRequest,
    selector: &Arc<Mutex<Selector>>,
) -> String {
    let selector = selector.lock().expect("question selector lock");
    let current = selector.current_id();
    let selected = selector.selected_ids();
    let multiple = request.mode == QuestionMode::Multiple;
    let other = offers_other(request)
        .then(|| QuestionOption::new(OTHER_ID, OTHER_LABEL).with_description(OTHER_DESCRIPTION));
    let mut rows = vec![request.prompt.clone()];
    for (index, option) in request.options.iter().chain(other.as_ref()).enumerate() {
        let marker = if current == Some(option.id.as_str()) {
            ">"
        } else {
            " "
        };
        let checked = if selected.contains(&option.id.as_str()) {
            "x"
        } else {
            " "
        };
        let lead = if multiple {
            format!("{marker} [{checked}] {}. ", index + 1)
        } else {
            format!("{marker} {}. ", index + 1)
        };
        let indent = " ".repeat(lead.chars().count());
        rows.push(format!("{lead}{}", option.label));
        if let Some(description) = option
            .description
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            rows.push(format!("{indent}{description}"));
        }
    }
    rows.push(if request.mode == QuestionMode::Multiple {
        "Up/Down moves; Space or number toggles; Enter answers; Esc cancels. ".to_string()
    } else {
        "Up/Down or number selects; Enter answers; Esc cancels. ".to_string()
    });
    rows.join("\n")
}

fn question_prompt(request: &QuestionRequest) -> String {
    let options = request
        .options
        .iter()
        .enumerate()
        .map(|(index, option)| format!("{}. {}", index + 1, option.label))
        .collect::<Vec<_>>()
        .join("; ");
    if options.is_empty() {
        format!("{}\n> ", request.prompt)
    } else {
        format!("{} [{}]\n> ", request.prompt, options)
    }
}

fn parse_question_answer(
    request: &QuestionRequest,
    raw: &str,
) -> Result<QuestionAnswer, ego_core::question::QuestionError> {
    let options = || {
        request
            .options
            .iter()
            .map(|option| SelectorOption::new(option.id.clone(), option.label.clone()))
    };
    let option_number = |value: &str| {
        value
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|number| *number > 0 && *number <= request.options.len())
            .or_else(|| {
                request
                    .options
                    .iter()
                    .position(|option| option.id == value.trim())
                    .map(|index| index + 1)
            })
    };
    let answer = match request.mode {
        QuestionMode::Single => {
            let mut selector = Selector::with_options(SelectorKind::Single, options());
            if let Some(number) = option_number(raw) {
                selector.key(SelectorKey::Number(number));
                selector.key(SelectorKey::Enter);
                match selector.result() {
                    Some(SelectorValue::Single(id)) => QuestionAnswer::single(id.clone()),
                    _ => QuestionAnswer::single(raw),
                }
            } else {
                QuestionAnswer::single(raw)
            }
        }
        QuestionMode::Multiple => {
            let mut selector = Selector::with_options(SelectorKind::Multiple, options());
            let mut free_text = Vec::new();
            for value in raw
                .split([',', ' '])
                .filter(|value| !value.trim().is_empty())
            {
                if let Some(number) = option_number(value) {
                    selector.key(SelectorKey::Number(number));
                } else {
                    free_text.push(value.trim().to_string());
                }
            }
            selector.key(SelectorKey::Enter);
            let mut selected = match selector.result() {
                Some(SelectorValue::Multiple(ids)) => ids.clone(),
                _ => Vec::new(),
            };
            selected.extend(free_text);
            QuestionAnswer::multiple(selected)
        }
        QuestionMode::Round(_) => return QuestionAnswer::from_json(request, raw),
        QuestionMode::FreeText => {
            let mut selector = Selector::free_text();
            selector.insert(raw);
            selector.key(SelectorKey::Enter);
            match selector.result() {
                Some(SelectorValue::FreeText(value)) => QuestionAnswer::free_text(value.clone()),
                _ => QuestionAnswer::free_text(raw),
            }
        }
        QuestionMode::Form { .. } => return QuestionAnswer::from_json(request, raw),
    };
    answer.validate_for(request)?;
    Ok(answer)
}

/// One read of the approval modal, whose prompt is retired as soon as it
/// returns: an answered approval must not keep showing answer keys, and the
/// next prompt (a follow-up question, the next approval or the composer)
/// starts on the same row. Its outcome is committed as one receipt row.
///
/// Nothing writes between the return and the erase, which is what lets
/// Reedline find the rows it painted. When it cannot (a resize while no read
/// was active), the prompt stays on screen above the receipt: a stale block is
/// preferable to erasing rows that may hold the transcript.
fn permission_line(
    editor: &mut Reedline,
    surface: &mut ModalSurface<'_>,
    prompt: &dyn Prompt,
) -> Line {
    let line = modal_line(editor, surface, prompt);
    let _ = editor.erase_last_prompt();
    line
}

fn modal_line(editor: &mut Reedline, surface: &mut ModalSurface<'_>, prompt: &dyn Prompt) -> Line {
    surface_line(
        editor,
        prompt,
        surface.phase,
        surface.status,
        surface.asked_for_image,
        surface.ingress,
        surface.drafts,
    )
    .0
}

fn amendment(
    editor: &mut Reedline,
    surface: &mut ModalSurface<'_>,
    request: &PermissionRequest,
    notice: &mut Option<String>,
) -> PermissionDecision {
    let Some(presentation) = request.presentation() else {
        surface.status.lock().expect("surface status lock").notice =
            Some("this permission request has no editable parameters".into());
        repaint_status(surface.status);
        return PermissionDecision::Deny;
    };
    let mut form = match PermissionEditForm::new(
        presentation.schema().clone(),
        presentation.parameters().clone(),
    ) {
        Ok(form) => form,
        Err(error) => {
            surface.status.lock().expect("surface status lock").notice = Some(error);
            repaint_status(surface.status);
            return PermissionDecision::Deny;
        }
    };
    loop {
        for index in 0..form.fields.len() {
            loop {
                let field = &form.fields[index];
                let prompt = format!("{}: ", field.name);
                match prefilled_modal_line(editor, surface, &prompt, &field.raw) {
                    Line::Submitted(raw) => match form.set_field(index, raw) {
                        Ok(()) => break,
                        Err(error) => inline_permission_error(surface.status, error),
                    },
                    Line::Blank => match form.set_field(index, String::new()) {
                        Ok(()) => break,
                        Err(error) => inline_permission_error(surface.status, error),
                    },
                    Line::Ended | Line::ExternalBreak(_) => {
                        return PermissionDecision::Deny;
                    }
                    Line::Interrupted
                    | Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                        *notice = Some(crate::repl::INTERRUPTED_RECEIPT.to_owned());
                        return PermissionDecision::Deny;
                    }
                    Line::Refused(_) | Line::HostCommand(_) => continue,
                }
            }
        }
        if form.has_fallback() {
            loop {
                let raw = form.fallback_json();
                match prefilled_modal_line(
                    editor,
                    surface,
                    "Nested or additional parameters (JSON): ",
                    &raw,
                ) {
                    Line::Submitted(raw) => match form.set_fallback(&raw) {
                        Ok(()) => break,
                        Err(error) => inline_permission_error(surface.status, error),
                    },
                    Line::Blank => match form.set_fallback("{}") {
                        Ok(()) => break,
                        Err(error) => inline_permission_error(surface.status, error),
                    },
                    Line::Ended | Line::ExternalBreak(_) => {
                        return PermissionDecision::Deny;
                    }
                    Line::Interrupted
                    | Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                        *notice = Some(crate::repl::INTERRUPTED_RECEIPT.to_owned());
                        return PermissionDecision::Deny;
                    }
                    Line::Refused(_) | Line::HostCommand(_) => continue,
                }
            }
        }
        match form.validate() {
            Ok(params) => return PermissionDecision::Amend { params },
            Err(error) => inline_permission_error(surface.status, error),
        }
    }
}

pub(super) enum PermissionEditOutcome {
    Value(serde_json::Map<String, serde_json::Value>),
    Cancelled,
    Propagate(Line),
}

fn read_permission_parameter(
    editor: &mut Reedline,
    status: &SharedStatus,
    prompt: String,
    value: String,
    blank: &str,
    mut set: impl FnMut(String) -> Result<(), String>,
) -> Result<(), Box<PermissionEditOutcome>> {
    loop {
        editor.run_edit_commands(&[EditCommand::Clear, EditCommand::InsertString(value.clone())]);
        let prompt = Question::new(prompt.clone(), Arc::clone(status));
        let result = match answer(false, editor.read_line(&prompt)) {
            Line::Submitted(raw) => set(raw),
            Line::Blank => set(blank.to_owned()),
            line @ (Line::Ended | Line::ExternalBreak(_)) => {
                return Err(Box::new(PermissionEditOutcome::Propagate(line)));
            }
            Line::Interrupted
            | Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                return Err(Box::new(PermissionEditOutcome::Cancelled));
            }
            Line::Refused(_) | Line::HostCommand(_) => continue,
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) => inline_permission_error(status, error),
        }
    }
}

fn read_permission_form(
    editor: &mut Reedline,
    status: &SharedStatus,
    form: &mut PermissionEditForm,
) -> Result<serde_json::Map<String, serde_json::Value>, Box<PermissionEditOutcome>> {
    loop {
        for index in 0..form.fields.len() {
            let field = &form.fields[index];
            let prompt = format!("{}: ", field.name);
            let value = field.raw.clone();
            read_permission_parameter(editor, status, prompt, value, "", |raw| {
                form.set_field(index, raw)
            })?;
        }
        if form.has_fallback() {
            let value = form.fallback_json();
            read_permission_parameter(
                editor,
                status,
                "Nested or additional parameters (JSON): ".into(),
                value,
                "{}",
                |raw| form.set_fallback(&raw),
            )?;
        }
        match form.validate() {
            Ok(serde_json::Value::Object(parameters)) => {
                return Ok(parameters);
            }
            Ok(_) => unreachable!("permission forms validate only objects"),
            Err(error) => inline_permission_error(status, error),
        }
    }
}

pub(super) fn edit_permission_parameters(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
    schema: serde_json::Value,
    parameters: serde_json::Value,
) -> PermissionEditOutcome {
    let mut form = match PermissionEditForm::new(schema, parameters) {
        Ok(form) => form,
        Err(error) => {
            inline_permission_error(status, error);
            return PermissionEditOutcome::Cancelled;
        }
    };
    let saved = enter_modal(editor, phase, status);
    let outcome = read_permission_form(editor, status, &mut form);
    leave_modal(
        editor,
        phase,
        status,
        saved,
        if busy {
            EditPhase::Busy
        } else {
            EditPhase::Idle
        },
    );
    match outcome {
        Ok(parameters) => PermissionEditOutcome::Value(parameters),
        Err(outcome) => *outcome,
    }
}

fn prefilled_modal_line(
    editor: &mut Reedline,
    surface: &mut ModalSurface<'_>,
    prompt: &str,
    value: &str,
) -> Line {
    editor.run_edit_commands(&[
        EditCommand::Clear,
        EditCommand::InsertString(value.to_owned()),
    ]);
    permission_line(
        editor,
        surface,
        &Question::new(prompt.to_owned(), Arc::clone(surface.status)),
    )
}

fn inline_permission_error(status: &SharedStatus, error: impl std::fmt::Display) {
    status.lock().expect("surface status lock").notice =
        Some(format!("invalid tool parameters: {error}"));
    repaint_status(status);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionFieldKind {
    String,
    Integer,
    Number,
    Boolean,
    StringArray,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionEditField {
    name: String,
    kind: PermissionFieldKind,
    nullable: bool,
    required: bool,
    was_present: bool,
    was_null: bool,
    raw: String,
}

struct PermissionEditForm {
    schema: serde_json::Value,
    fields: Vec<PermissionEditField>,
    fallback: serde_json::Map<String, serde_json::Value>,
    fallback_available: bool,
}

impl PermissionEditForm {
    fn new(schema: serde_json::Value, parameters: serde_json::Value) -> Result<Self, String> {
        let serde_json::Value::Object(parameters) = parameters else {
            return Err("editable tool parameters must be a JSON object".into());
        };
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        let required = schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .collect::<std::collections::HashSet<_>>();
        let mut fallback = parameters.clone();
        let mut fields = Vec::new();
        let mut unsupported = false;
        for (name, property) in &properties {
            let Some((kind, nullable)) = permission_field_kind(name, property) else {
                unsupported = true;
                if required.contains(name.as_str()) && !fallback.contains_key(name) {
                    fallback.insert(name.clone(), serde_json::Value::Null);
                }
                continue;
            };
            let current = parameters.get(name);
            let raw = match render_permission_field(kind, current) {
                Some(raw) => raw,
                None => {
                    unsupported = true;
                    continue;
                }
            };
            fallback.remove(name);
            fields.push(PermissionEditField {
                name: name.clone(),
                kind,
                nullable,
                required: required.contains(name.as_str()),
                was_present: current.is_some(),
                was_null: current == Some(&serde_json::Value::Null),
                raw,
            });
        }
        Ok(Self {
            schema,
            fields,
            fallback,
            fallback_available: unsupported || properties.is_empty(),
        })
    }

    fn set_field(&mut self, index: usize, raw: String) -> Result<(), String> {
        let field = self
            .fields
            .get_mut(index)
            .ok_or_else(|| "the edited field is no longer available".to_owned())?;
        parse_permission_field(field, &raw)?;
        field.raw = raw;
        Ok(())
    }

    fn has_fallback(&self) -> bool {
        self.fallback_available || !self.fallback.is_empty()
    }

    fn fallback_json(&self) -> String {
        serde_json::to_string_pretty(&self.fallback).expect("a JSON object always serializes")
    }

    fn set_fallback(&mut self, raw: &str) -> Result<(), String> {
        self.fallback = serde_json::from_str(raw)
            .map_err(|error| format!("nested parameters are not a JSON object: {error}"))?;
        Ok(())
    }

    fn value(&self) -> Result<serde_json::Value, String> {
        let mut value = self.fallback.clone();
        for field in &self.fields {
            if let Some(parsed) = parse_permission_field(field, &field.raw)? {
                value.insert(field.name.clone(), parsed);
            } else {
                value.remove(&field.name);
            }
        }
        Ok(serde_json::Value::Object(value))
    }

    fn validate(&self) -> Result<serde_json::Value, String> {
        let value = self.value()?;
        let request = QuestionRequest::new(
            "Edit tool parameters",
            QuestionMode::Form {
                schema: self.schema.clone(),
            },
            Vec::new(),
            false,
        );
        let raw = serde_json::to_string(&value).expect("JSON parameters always serialize");
        QuestionAnswer::from_json(&request, &raw)
            .map(|_| value)
            .map_err(|error| error.to_string())
    }
}

fn permission_field_kind(
    name: &str,
    schema: &serde_json::Value,
) -> Option<(PermissionFieldKind, bool)> {
    let mut types = match schema.get("type")? {
        serde_json::Value::String(kind) => vec![kind.as_str()],
        serde_json::Value::Array(kinds) => {
            kinds.iter().filter_map(serde_json::Value::as_str).collect()
        }
        _ => return None,
    };
    let nullable = types.contains(&"null");
    types.retain(|kind| *kind != "null");
    let kind = match types.as_slice() {
        ["string"] => PermissionFieldKind::String,
        ["integer"] => PermissionFieldKind::Integer,
        ["number"] => PermissionFieldKind::Number,
        ["boolean"] => PermissionFieldKind::Boolean,
        ["array"]
            if name == "argv"
                && schema
                    .pointer("/items/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("string") =>
        {
            PermissionFieldKind::StringArray
        }
        _ => return None,
    };
    Some((kind, nullable))
}

fn render_permission_field(
    kind: PermissionFieldKind,
    value: Option<&serde_json::Value>,
) -> Option<String> {
    match value {
        None | Some(serde_json::Value::Null) => Some(String::new()),
        Some(serde_json::Value::String(value)) if kind == PermissionFieldKind::String => {
            Some(value.clone())
        }
        Some(serde_json::Value::Number(value))
            if matches!(
                kind,
                PermissionFieldKind::Integer | PermissionFieldKind::Number
            ) =>
        {
            Some(value.to_string())
        }
        Some(serde_json::Value::Bool(value)) if kind == PermissionFieldKind::Boolean => {
            Some(value.to_string())
        }
        Some(serde_json::Value::Array(values)) if kind == PermissionFieldKind::StringArray => {
            let values = values
                .iter()
                .map(serde_json::Value::as_str)
                .collect::<Option<Vec<_>>>()?;
            Some(shell_words::join(values))
        }
        _ => None,
    }
}

fn parse_permission_field(
    field: &PermissionEditField,
    raw: &str,
) -> Result<Option<serde_json::Value>, String> {
    if raw.is_empty() && !field.required && !field.was_present {
        return Ok(None);
    }
    if raw.is_empty() && field.was_null && field.nullable {
        return Ok(Some(serde_json::Value::Null));
    }
    let value = match field.kind {
        PermissionFieldKind::String => serde_json::Value::String(raw.to_owned()),
        PermissionFieldKind::Integer => serde_json::Value::Number(
            raw.parse::<i64>()
                .map_err(|_| format!("{} must be an integer", field.name))?
                .into(),
        ),
        PermissionFieldKind::Number => {
            let value = raw
                .parse::<f64>()
                .map_err(|_| format!("{} must be a number", field.name))?;
            serde_json::Number::from_f64(value)
                .map(serde_json::Value::Number)
                .ok_or_else(|| format!("{} must be a finite number", field.name))?
        }
        PermissionFieldKind::Boolean => serde_json::Value::Bool(
            raw.parse::<bool>()
                .map_err(|_| format!("{} must be true or false", field.name))?,
        ),
        PermissionFieldKind::StringArray => serde_json::Value::Array(
            shell_words::split(raw)
                .map_err(|error| format!("{} is not a valid argument line: {error}", field.name))?
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    };
    Ok(Some(value))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PermissionWord {
    Allow,
    Deny,
    Amend,
    Remember,
}

pub(super) fn parse_trust(answer: &str) -> Option<Trust> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(Trust::Trusted),
        "n" | "no" => Some(Trust::Untrusted),
        _ => None,
    }
}

pub(super) fn parse_permission(answer: &str) -> Option<PermissionWord> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "a" | "allow" | "y" | "yes" => Some(PermissionWord::Allow),
        "d" | "deny" | "n" | "no" => Some(PermissionWord::Deny),
        "m" | "amend" | "e" | "edit" => Some(PermissionWord::Amend),
        "r" | "remember" => Some(PermissionWord::Remember),
        _ => None,
    }
}

fn remember_permission(
    status: &SharedStatus,
    request: &PermissionRequest,
) -> Result<String, String> {
    let presentation = request
        .presentation()
        .ok_or_else(|| "this permission request has no presentation facts".to_owned())?;
    let plan = presentation
        .suggested_remember_plan()
        .map_err(str::to_owned)?
        .clone();
    let application = status
        .lock()
        .map_err(|_| "the permission surface is unavailable".to_owned())?
        .permission_rules
        .clone()
        .ok_or_else(|| "the atomic permission-rule application is unavailable".to_owned())?;
    application
        .remember_allow(plan)
        .map_err(|error| error.to_string())
}

/// Write one out-of-band signal straight to the terminal.
///
/// Called only from the terminal owner, between reads, so nothing else is
/// painting. It does not go through the row output: that holds text until a
/// newline, and a wait has no more rows coming.
fn tell_host(status: &SharedStatus, signal: impl FnOnce(Osc, bool) -> String) {
    let signals = status.lock().expect("surface status lock").signals;
    let Some((osc, quiet)) = signals else {
        return;
    };
    let bytes = signal(osc, quiet);
    if bytes.is_empty() {
        return;
    }
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(bytes.as_bytes());
    let _ = stdout.flush();
}

/// Resolve one answered key or word. `notice` receives the receipt that goes
/// to the transcript.
fn settle_permission(
    editor: &mut Reedline,
    surface: &mut ModalSurface<'_>,
    request: &PermissionRequest,
    word: PermissionWord,
    notice: &mut Option<String>,
) -> PermissionDecision {
    match word {
        PermissionWord::Allow => PermissionDecision::Allow,
        PermissionWord::Deny => PermissionDecision::Deny,
        PermissionWord::Amend => {
            surface
                .status
                .lock()
                .expect("surface status lock")
                .permission_modal = false;
            amendment(editor, surface, request, notice)
        }
        PermissionWord::Remember => match remember_permission(surface.status, request) {
            Ok(receipt) => {
                *notice = Some(receipt);
                PermissionDecision::Allow
            }
            Err(error) => {
                *notice = Some(format!("Not remembered, and the call was refused: {error}"));
                PermissionDecision::Deny
            }
        },
    }
}

/// `n`: ask what to do instead. An empty answer is a plain refusal; words go
/// back to the model as the tool result.
fn decline(editor: &mut Reedline, surface: &mut ModalSurface<'_>) -> PermissionDecision {
    surface
        .status
        .lock()
        .expect("surface status lock")
        .permission_modal = false;
    let prompt = Question::new(
        "What should it do instead? (Enter alone just says no) ".to_owned(),
        Arc::clone(surface.status),
    );
    match permission_line(editor, surface, &prompt) {
        Line::Submitted(text) if !text.trim().is_empty() => {
            PermissionDecision::DenyWithInstruction {
                instruction: text.trim().to_owned(),
            }
        }
        _ => PermissionDecision::Deny,
    }
}

/// One row into the existing output pump; only the pump waits on Reedline.
pub(super) fn commit_to_transcript(status: &SharedStatus, row: &str) {
    let transcript = status
        .lock()
        .expect("surface status lock")
        .transcript
        .clone();
    if let Some(transcript) = transcript
        && transcript
            .send(SurfaceMessage::Row(format!("{row}\n")))
            .is_err()
    {
        status.lock().expect("surface status lock").notice =
            Some("Permission receipt could not reach the output pump".to_owned());
    }
}

/// The prompt: two characters and nothing else.
///
/// No model name, no token count, no cwd. Everything a status line could say
/// is either already on the screen or answerable by asking, and a prompt that
/// redraws a changing right-hand side is a second writer competing with the
/// renderer for the same row.
pub(super) struct Waiting {
    status: SharedStatus,
}

impl Waiting {
    pub(super) fn new(status: SharedStatus) -> Self {
        Self { status }
    }
}

fn status_prefix(status: &SurfaceStatus) -> String {
    let mut rows = Vec::new();
    rows.extend(
        status
            .image_chips
            .iter()
            .map(|name| format!("[image: {name}]")),
    );
    if !status.native_status {
        if status.queue.pending_steers > 0 {
            rows.push(QUEUE_LABEL.to_string());
        }
        if status.queue.interrupt_pending {
            rows.push("interrupt pending".to_string());
        }
    }
    if let Some(notice) = status.notice.as_ref() {
        rows.push(notice.clone());
    }
    if rows.is_empty() {
        String::new()
    } else if rows.iter().any(|row| row.contains('\n')) {
        format!("{}\n", rows.join("\n"))
    } else {
        format!("{} | ", rows.join(" | "))
    }
}

fn status_right(status: &SharedStatus) -> String {
    let painter = Arc::clone(&status.lock().expect("surface status lock").painter);
    painter
        .current_status()
        .map(|line| line.text().to_string())
        .unwrap_or_default()
}

fn status_row(status: &SurfaceStatus) -> String {
    if let Some((message, started, delay_ms)) = &status.retry {
        let remaining = delay_ms.saturating_sub(started.elapsed().as_millis() as u64);
        let message = if let Some((cause, rest)) =
            message.split_once(" — connection problem, retrying in ")
        {
            let counter = rest
                .split_once(" (")
                .map(|(_, counter)| counter)
                .unwrap_or("");
            format!(
                "{cause} — connection problem, retrying in {:.1}s ({counter}",
                remaining as f64 / 1000.0
            )
        } else {
            message.clone()
        };
        let message = message.replace(['\r', '\n', '\t'], " ");
        let mut line = crate::render::line::Line::empty();
        line.push(crate::render::line::Span::plain(message));
        return format!(
            "{}\n",
            line.truncate_to(status.permission_capabilities.width.saturating_sub(1))
                .text()
        );
    }
    if status.activity {
        let elapsed = status
            .activity_since
            .map(|started| started.elapsed().as_secs())
            .unwrap_or(0);
        return format!(
            "{} Working {elapsed}s \x1b[2m· Esc to interrupt\x1b[22m\n",
            ACTIVITY_FRAMES[(elapsed as usize) % ACTIVITY_FRAMES.len()]
        );
    }
    let text = status
        .painter
        .current_status()
        .map(|line| line.text().to_string())
        .unwrap_or_default();
    if text.is_empty() {
        String::new()
    } else {
        format!("{text}\n")
    }
}

impl Prompt for Waiting {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        let status = self.status.lock().expect("surface status lock");
        let row = status_row(&status);
        Cow::Owned(format!("{row}{}> ", status_prefix(&status)))
    }

    /// Alt+Enter and a bracketed paste both put a real newline in the buffer,
    /// and this is the only mark that the line being typed is still one line.
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("… ")
    }

    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Owned(format!("(search: {}) ", search.term))
    }
}

/// Kept wholly in the indicator because Reedline's large-buffer path measures
/// wrapped prompt rows but skips `prompt_str_left` by logical newlines. A long
/// one-line question can therefore be measured off-screen and then sliced to
/// empty. Modal content deliberately does not borrow the live-status prefix:
/// the status row and the question have independent ownership and a changing
/// notice must never become part of an approval prompt.
pub(super) struct Question {
    text: String,
    status: SharedStatus,
}

impl Question {
    pub(super) fn new(text: String, status: SharedStatus) -> Self {
        Self { text, status }
    }
}

impl Prompt for Question {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        if self
            .status
            .lock()
            .expect("surface status lock")
            .native_status
        {
            Cow::Borrowed("")
        } else {
            Cow::Owned(status_right(&self.status))
        }
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed(&self.text)
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("… ")
    }

    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Owned(format!("(search: {}) ", search.term))
    }
}

#[cfg(test)]
mod ask_user_render_tests {
    use super::*;

    #[test]
    fn ask_user_answer_does_not_share_the_last_question_row() {
        let long_question = format!(
            "Provide Verification connection: {}Do not send passwords or tokens.",
            "Describe the verification connection and its public configuration. ".repeat(4)
        );
        let cases = [
            QuestionRequest::new("Which connection?", QuestionMode::FreeText, vec![], true),
            QuestionRequest::new(long_question, QuestionMode::FreeText, vec![], true),
            QuestionRequest::new(
                "Provide connection details.\nDo not send passwords or tokens.",
                QuestionMode::FreeText,
                vec![],
                true,
            ),
            QuestionRequest::new(
                "Which connection?",
                QuestionMode::Single,
                vec![QuestionOption::new("public", "Public connection")],
                false,
            ),
            QuestionRequest::new(
                "Provide connection JSON.",
                QuestionMode::Form {
                    schema: serde_json::json!({"type": "object"}),
                },
                vec![],
                false,
            ),
        ];
        for request in cases {
            let status = Arc::new(Mutex::new(SurfaceStatus::default()));
            let prompt = Question::new(question_prompt(&request), status);
            let rendered = format!(
                "{}{}typed-answer",
                prompt.render_prompt_left(),
                prompt.render_prompt_indicator(PromptEditMode::Emacs),
            );
            let mut terminal = vt100::Parser::new(24, 120, 0);
            terminal.process(rendered.replace('\n', "\r\n").as_bytes());
            let rows = terminal.screen().rows(0, 120).collect::<Vec<_>>();
            let answer_row = rows
                .iter()
                .position(|row| row.contains("typed-answer"))
                .expect("the typed answer is visible");
            assert_eq!(
                rows[answer_row], "> typed-answer",
                "answer must occupy its own marked row: {rendered:?}"
            );
            let mut question_screen = vt100::Parser::new(24, 120, 0);
            let question = if request.options.is_empty() {
                request.prompt.clone()
            } else {
                format!("{} [1. Public connection]", request.prompt)
            };
            question_screen.process(question.replace('\n', "\r\n").as_bytes());
            let last_question_row = question_screen.screen().cursor_position().0 as usize;
            assert_eq!(answer_row, last_question_row + 1);
            assert_eq!(
                &rows[..answer_row],
                &question_screen
                    .screen()
                    .rows(0, 120)
                    .take(answer_row)
                    .collect::<Vec<_>>(),
                "the complete question and options must remain above the answer"
            );
        }
    }
}

#[cfg(test)]
mod permission_edit_form_tests {
    use super::*;
    use serde_json::json;

    fn exec_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["argv"],
            "properties": {
                "argv": { "type": "array", "items": { "type": "string" } },
                "cwd": { "type": ["string", "null"] },
                "timeout_ms": { "type": ["integer", "null"], "minimum": 1 },
                "environment": { "type": "object" }
            },
            "additionalProperties": false
        })
    }

    #[test]
    fn exec_arguments_are_structured_and_nested_values_stay_in_prefilled_fallback() {
        let form = PermissionEditForm::new(
            exec_schema(),
            json!({
                "argv": ["wiz-run", "stories-cli.js", "show", "regression_case"],
                "cwd": "/workspace",
                "timeout_ms": 30000,
                "environment": { "LANG": "C" }
            }),
        )
        .unwrap();

        assert_eq!(
            form.fields
                .iter()
                .map(|field| (field.name.as_str(), field.kind, field.raw.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "argv",
                    PermissionFieldKind::StringArray,
                    "wiz-run stories-cli.js show regression_case"
                ),
                ("cwd", PermissionFieldKind::String, "/workspace"),
                ("timeout_ms", PermissionFieldKind::Integer, "30000"),
            ]
        );
        assert_eq!(
            serde_json::Value::Object(form.fallback.clone()),
            json!({ "environment": { "LANG": "C" } })
        );
        assert!(form.has_fallback());
        assert_eq!(
            form.validate().unwrap(),
            json!({
                "argv": ["wiz-run", "stories-cli.js", "show", "regression_case"],
                "cwd": "/workspace",
                "timeout_ms": 30000,
                "environment": { "LANG": "C" }
            })
        );
    }

    #[test]
    fn field_edits_parse_without_json_and_schema_errors_remain_typed() {
        let mut form = PermissionEditForm::new(
            exec_schema(),
            json!({ "argv": ["cargo", "test"], "cwd": null, "timeout_ms": null }),
        )
        .unwrap();
        let argv = form
            .fields
            .iter()
            .position(|field| field.name == "argv")
            .unwrap();
        let timeout = form
            .fields
            .iter()
            .position(|field| field.name == "timeout_ms")
            .unwrap();

        form.set_field(argv, "cargo test -p ego-core".into())
            .unwrap();
        assert!(form.set_field(timeout, "soon".into()).is_err());
        form.set_field(timeout, "0".into()).unwrap();
        assert!(form.validate().unwrap_err().contains("form answer"));
        form.set_field(timeout, "1000".into()).unwrap();
        assert_eq!(
            form.validate().unwrap()["argv"],
            json!(["cargo", "test", "-p", "ego-core"])
        );
    }
}

/// The terminal owner invokes this only after fresh explicit human consent.
fn launch_elicitation_url(url: &str) -> bool {
    if ego_core::question::validate_url(url).is_err() {
        return false;
    }
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = std::process::Command::new("xdg-open");
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
