use super::*;

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
        status.origin = saved.origin;
        status.notice = None;
        if let Some(snapshot) = status.snapshot.as_mut() {
            snapshot.set_modal(false);
        }
    }
    phase.set(after);
    repaint_status(status);
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
        Permission(oneshot::Sender<PermissionDecision>, PermissionDecision),
        Question(oneshot::Sender<QuestionOutcome>, QuestionOutcome),
    }

    let (reply, after, mut keep_running) = match control {
        Control::ConfirmCompaction { source, reply } => {
            let confirmed = if surface.drafts.active() != source {
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
                            | Line::HostCommand(
                                HostCommand::Interrupt | HostCommand::ModalEscape,
                            ) => break false,
                            Line::Refused(_) | Line::HostCommand(_) => continue,
                        }
                    }
                }
            };
            (Reply::Compaction(reply, confirmed), EditPhase::Idle, true)
        }
        Control::Trust { workspace, reply } => {
            let prompt = Question::new(
                format!("Trust project files in {workspace}? [y/N] "),
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
            let prompt = Question::new(
                format!("{request}. [allow/deny/amend] "),
                Arc::clone(surface.status),
            );
            let decision = loop {
                match modal_line(editor, surface, &prompt) {
                    Line::Submitted(answer) => match parse_permission(&answer) {
                        Some(PermissionWord::Allow) => break PermissionDecision::Allow,
                        Some(PermissionWord::Deny) => break PermissionDecision::Deny,
                        Some(PermissionWord::Amend) => {
                            break amendment(editor, surface);
                        }
                        None => continue,
                    },
                    Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => {
                        break PermissionDecision::Deny;
                    }
                    Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                        break PermissionDecision::Deny;
                    }
                    Line::Blank | Line::Refused(_) | Line::HostCommand(_) => continue,
                }
            };
            (Reply::Permission(reply, decision), EditPhase::Busy, true)
        }
        Control::Question {
            id: _id,
            request,
            reply,
        } => {
            let outcome = answer_question(editor, surface, &request);
            (Reply::Question(reply, outcome), EditPhase::Busy, true)
        }
        Control::Shutdown => return false,
        Control::ActivateSession { .. } => unreachable!("activation is handled before modal entry"),
        Control::Activate { .. } => unreachable!("activation is handled before modal entry"),
        Control::ConfirmRemember { .. } => {
            unreachable!("remember confirmation is handled before modal entry")
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
        Reply::Permission(sender, decision) => {
            let _ = sender.send(decision);
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
            .map(|option| SelectorOption::new(option.id.clone(), option.label.clone())),
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
        match modal_line(editor, surface, &prompt) {
            Line::HostCommand(HostCommand::QuestionSelector(command)) => {
                let Some(key) = question_selector_key(&command) else {
                    continue;
                };
                let mut selector = selector.lock().expect("question selector lock");
                selector.key(key);
                if key == SelectorKey::Escape {
                    break QuestionOutcome::Cancelled;
                }
                if key != SelectorKey::Enter {
                    drop(selector);
                    surface.status.lock().expect("surface status lock").notice = None;
                    repaint_status(surface.status);
                    continue;
                }
                let answer = match selector.result() {
                    Some(SelectorValue::Single(id)) => QuestionAnswer::single(id.clone()),
                    Some(SelectorValue::Multiple(ids)) => QuestionAnswer::multiple(ids.clone()),
                    Some(SelectorValue::FreeText(_)) | None => continue,
                };
                drop(selector);
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

pub(super) fn question_selector_key(command: &str) -> Option<SelectorKey> {
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

pub(super) fn question_selector_prompt(
    request: &QuestionRequest,
    selector: &Arc<Mutex<Selector>>,
) -> String {
    let selector = selector.lock().expect("question selector lock");
    let current = selector.current_id();
    let selected = selector.selected_ids();
    let mut rows = vec![request.prompt.clone()];
    rows.extend(request.options.iter().enumerate().map(|(index, option)| {
        let marker = if current == Some(option.id.as_str()) {
            ">"
        } else {
            " "
        };
        if request.mode == QuestionMode::Multiple {
            let checked = if selected.contains(&option.id.as_str()) {
                "x"
            } else {
                " "
            };
            format!("{marker} [{checked}] {}. {}", index + 1, option.label)
        } else {
            format!("{marker} {}. {}", index + 1, option.label)
        }
    }));
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
        format!("{} ", request.prompt)
    } else {
        format!("{} [{}] ", request.prompt, options)
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

fn amendment(editor: &mut Reedline, surface: &mut ModalSurface<'_>) -> PermissionDecision {
    let prompt = Question::new(
        "New JSON parameters: ".to_string(),
        Arc::clone(surface.status),
    );
    loop {
        match modal_line(editor, surface, &prompt) {
            Line::Submitted(raw) => match serde_json::from_str(&raw) {
                Ok(params) => return PermissionDecision::Amend { params },
                Err(_) => continue,
            },
            Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => {
                return PermissionDecision::Deny;
            }
            Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                return PermissionDecision::Deny;
            }
            Line::Blank | Line::Refused(_) | Line::HostCommand(_) => continue,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PermissionWord {
    Allow,
    Deny,
    Amend,
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
        "a" | "allow" => Some(PermissionWord::Allow),
        "d" | "deny" => Some(PermissionWord::Deny),
        "m" | "amend" => Some(PermissionWord::Amend),
        _ => None,
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

fn status_prefix(status: &SharedStatus) -> String {
    let status = status.lock().expect("surface status lock");
    let mut rows = Vec::new();
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

impl Prompt for Waiting {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Owned(status_right(&self.status))
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Owned(format!("{}> ", status_prefix(&self.status)))
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
/// empty. The indicator uses the post-prompt offset and preserves the complete
/// canonical request and answer vocabulary at narrow widths.
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
        Cow::Owned(status_right(&self.status))
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Owned(format!("{}{}", status_prefix(&self.status), self.text))
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("… ")
    }

    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Owned(format!("(search: {}) ", search.term))
    }
}
