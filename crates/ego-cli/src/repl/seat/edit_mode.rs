use super::*;

pub struct SurfaceEditMode {
    editor: Box<dyn EditMode>,
    modal_editor: Emacs,
    modal_command: bool,
    phase: SurfacePhase,
    status: SharedStatus,
    asked_for_image: Arc<AtomicBool>,
}

impl SurfaceEditMode {
    pub fn new(editor: impl EditMode + 'static, phase: SurfacePhase) -> Self {
        Self::with_status(
            editor,
            phase,
            Arc::new(Mutex::new(SurfaceStatus::with_registry(Arc::new(
                ActionRegistry::final_surface(),
            )))),
        )
    }

    pub(super) fn with_status(
        editor: impl EditMode + 'static,
        phase: SurfacePhase,
        status: SharedStatus,
    ) -> Self {
        Self::with_editor(Box::new(editor), phase, status)
    }

    pub(super) fn with_editor(
        editor: Box<dyn EditMode>,
        phase: SurfacePhase,
        status: SharedStatus,
    ) -> Self {
        Self {
            editor,
            modal_editor: emacs_composer(),
            modal_command: false,
            phase,
            status,
            asked_for_image: Arc::new(AtomicBool::new(false)),
        }
    }

    fn picker_open(&self) -> bool {
        if self.phase.get() == EditPhase::Modal {
            let picker = self
                .status
                .lock()
                .expect("surface status lock")
                .model_picker
                .clone();
            self.status
                .lock()
                .expect("surface status lock")
                .session_picker_open
                || picker.is_some_and(|picker| picker.lock().expect("model picker lock").is_open())
        } else {
            false
        }
    }

    pub(super) fn asked_for_image(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.asked_for_image)
    }
}

fn pasted_image(text: &str) -> bool {
    let path = std::path::Path::new(text);
    let image_extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["png", "jpg", "jpeg", "gif", "webp"]
                .iter()
                .any(|image| extension.eq_ignore_ascii_case(image))
        });
    // A file this process cannot open (permissions, a race) is
    // still staged: the model-capability refusal or the read
    // error surfaces downstream, before or without any bytes
    // being sent. Only a header this process *did* read and that
    // fails to decode as a supported image falls back to text.
    image_extension && path.is_file() && {
        use std::io::Read;
        let mut header = [0_u8; 12];
        match std::fs::File::open(path).and_then(|mut file| file.read(&mut header)) {
            Ok(read) => crate::attachment::media_type(&header[..read]).is_some(),
            Err(_) => true,
        }
    }
}

impl EditMode for SurfaceEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let raw: Event = event.into();

        // Modal keys belong only to that modal. Never change authority behind it.
        if let Event::Key(key) = &raw
            && key.code == KeyCode::BackTab
            && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
        {
            return if self.phase.get() == EditPhase::Modal {
                ReedlineEvent::None
            } else {
                ReedlineEvent::ExecuteHostCommand(MODE_CYCLE_COMMAND.to_owned())
            };
        }
        let picker_open = self.picker_open();
        let question_selector_open = if self.phase.get() == EditPhase::Modal {
            self.status
                .lock()
                .expect("surface status lock")
                .question_selector
                .is_some()
        } else {
            false
        };
        let permission_modal = self.phase.get() == EditPhase::Modal
            && self
                .status
                .lock()
                .expect("surface status lock")
                .permission_modal;

        if self.phase.get() != EditPhase::Modal && dismisses_notice(&raw) {
            self.status.lock().expect("surface status lock").notice = None;
        }

        match &raw {
            Event::Paste(text) if !picker_open && !question_selector_open => {
                if pasted_image(text) {
                    return ReedlineEvent::ExecuteHostCommand(format!(
                        "{PASTE_PATH_COMMAND}{text}"
                    ));
                }
                self.status.lock().expect("surface status lock").origin =
                    InputOrigin::BracketedPaste;
            }
            // Every phase, a modal included, tells the painter: a prompt
            // painted with the old width wraps, and is erased, on wrong rows.
            Event::Resize(width, height) => {
                self.status.lock().expect("surface status lock").width = usize::from(*width);
                repaint_status(&self.status);
                return ReedlineEvent::Resize(*width, *height);
            }
            _ => {}
        }

        if let Event::Key(key) = &raw
            && suspend::is_suspend_key(key)
        {
            // A suspend leaves whatever is on screen unanswered: the modals
            // re-render from their own state when the repaint arrives.
            return match suspend::suspend_job() {
                Ok((width, height)) => {
                    self.status.lock().expect("surface status lock").width = usize::from(width);
                    ReedlineEvent::Resize(width, height)
                }
                Err(reason) => {
                    self.status.lock().expect("surface status lock").notice =
                        Some(reason.to_owned());
                    ReedlineEvent::Repaint
                }
            };
        }

        // Key-only selectors still offer the same slash-command composer.
        // This mode owns only the temporary command text, never the selection.
        if self.phase.get() != EditPhase::Modal {
            self.modal_command = false;
        }
        if (picker_open || question_selector_open || permission_modal)
            && matches!(&raw, Event::Key(key) if key.code == KeyCode::Char('/')
                && key.modifiers.difference(KeyModifiers::SHIFT).is_empty())
        {
            self.modal_command = true;
        }
        if self.modal_command {
            if matches!(&raw, Event::Key(key) if key.code == KeyCode::Esc) {
                self.modal_command = false;
                return ReedlineEvent::Multiple(vec![
                    ReedlineEvent::Edit(vec![EditCommand::Clear]),
                    ReedlineEvent::Repaint,
                ]);
            }
            if matches!(&raw, Event::Key(key) if key.code == KeyCode::Enter) {
                self.modal_command = false;
            }
            return match ReedlineRawEvent::try_from(raw) {
                Ok(event) => self.modal_editor.parse_event(event),
                Err(()) => ReedlineEvent::None,
            };
        }

        if picker_open {
            let key = |command: &str| {
                self.status
                    .lock()
                    .expect("surface status lock")
                    .picker_keys
                    .push(command.to_owned());
                picker_command(PICKER_KEYS)
            };
            return match &raw {
                Event::Key(key_event)
                    if key_event
                        .modifiers
                        .difference(KeyModifiers::SHIFT)
                        .is_empty() =>
                {
                    match key_event.code {
                        KeyCode::Up => key("up"),
                        KeyCode::Down => key("down"),
                        KeyCode::Enter => key("enter"),
                        KeyCode::Esc => key("escape"),
                        KeyCode::Backspace => key("backspace"),
                        KeyCode::Char(digit @ '1'..='9') => key(&format!("number:{digit}")),
                        KeyCode::Char(character) => key(&format!("char:{character}")),
                        _ => ReedlineEvent::None,
                    }
                }
                Event::Key(key_event)
                    if key_event.code == KeyCode::Char('c')
                        && key_event.modifiers == KeyModifiers::CONTROL =>
                {
                    key("escape")
                }
                _ => ReedlineEvent::None,
            };
        }

        if question_selector_open {
            let queued = |command: &str| {
                self.status
                    .lock()
                    .expect("surface status lock")
                    .picker_keys
                    .push(command.to_owned());
                question_selector_command(PICKER_KEYS)
            };
            return match &raw {
                Event::Key(key) if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                    match key.code {
                        KeyCode::Up => queued("up"),
                        KeyCode::Down => queued("down"),
                        KeyCode::Enter => queued("enter"),
                        KeyCode::Esc => queued("escape"),
                        KeyCode::Char(' ') => queued("space"),
                        KeyCode::Char(digit @ '1'..='9') => queued(&format!("number:{digit}")),
                        _ => ReedlineEvent::None,
                    }
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL =>
                {
                    queued("escape")
                }
                _ => ReedlineEvent::None,
            };
        }

        if permission_modal {
            return match &raw {
                Event::Key(key)
                    if key.code == KeyCode::Char('b') && key.modifiers == KeyModifiers::CONTROL =>
                {
                    ReedlineEvent::ExecuteHostCommand(BACKGROUND_COMMAND.to_string())
                }
                Event::Key(key) if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                    match key.code {
                        KeyCode::Char('y' | 'Y') => permission_decision_command("allow"),
                        KeyCode::Char('n' | 'N') => permission_decision_command("deny"),
                        KeyCode::Char('e' | 'E') => permission_decision_command("amend"),
                        KeyCode::Char('a' | 'A') => permission_decision_command("remember"),
                        KeyCode::Char('d' | 'D') => permission_decision_command("details"),
                        KeyCode::Enter => ReedlineEvent::None,
                        KeyCode::Esc => {
                            ReedlineEvent::ExecuteHostCommand(MODAL_ESCAPE_COMMAND.to_string())
                        }
                        _ => ReedlineEvent::None,
                    }
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL =>
                {
                    ReedlineEvent::ExecuteHostCommand(MODAL_ESCAPE_COMMAND.to_string())
                }
                _ => ReedlineEvent::None,
            };
        }

        if let Event::Key(key) = &raw {
            if self.phase.get() == EditPhase::Idle
                && key.code == KeyCode::Esc
                && key.modifiers.is_empty()
                && !self
                    .status
                    .lock()
                    .expect("surface status lock")
                    .image_chips
                    .is_empty()
            {
                return ReedlineEvent::ExecuteHostCommand(REMOVE_PASTE_IMAGE_COMMAND.to_owned());
            }
            if self.phase.get() == EditPhase::Modal
                && key.code == reedline::KeyCode::Esc
                && key.modifiers.is_empty()
            {
                return ReedlineEvent::ExecuteHostCommand(MODAL_ESCAPE_COMMAND.to_string());
            }
            if self.phase.get() == EditPhase::Modal {
                return match ReedlineRawEvent::try_from(raw) {
                    Ok(event) => self.modal_editor.parse_event(event),
                    Err(()) => ReedlineEvent::None,
                };
            }
            let action = self
                .status
                .lock()
                .expect("surface status lock")
                .actions
                .as_ref()
                .and_then(|actions| actions.registry.resolve_key(*key));
            if let Some(action) = action {
                let name = self
                    .status
                    .lock()
                    .expect("surface status lock")
                    .actions
                    .as_ref()
                    .map(|actions| actions.registry.help(action).name().to_string())
                    .unwrap_or_default();
                return ReedlineEvent::ExecuteHostCommand(format!("{ACTION_COMMAND}{name}"));
            }
            if key.code == KeyCode::Up
                && key.modifiers.is_empty()
                && self.phase.get() == EditPhase::Busy
                && {
                    let status = self.status.lock().expect("surface status lock");
                    status.queue.pending_steers + status.queue.queued > 0
                }
            {
                let fallback = match ReedlineRawEvent::try_from(raw.clone()) {
                    Ok(event) => self.editor.parse_event(event),
                    Err(()) => ReedlineEvent::None,
                };
                return ReedlineEvent::UntilFound(vec![
                    ReedlineEvent::ExecuteHostCommandIfFirstLine(BUSY_RECALL_COMMAND.to_owned()),
                    fallback,
                ]);
            }
            match intercept(key.code, key.modifiers) {
                Intercept::ClipboardImage => {
                    self.asked_for_image.store(true, Ordering::Relaxed);
                    return ReedlineEvent::ExecuteHostCommand(String::new());
                }
                Intercept::Draft => {
                    return ReedlineEvent::ExecuteHostCommand(DRAFT_COMMAND.to_string());
                }
                Intercept::SubmitBusy => {
                    return ReedlineEvent::ExecuteHostCommand(BUSY_SUBMIT_COMMAND.to_string());
                }
                Intercept::SteerBusy if self.phase.get() == EditPhase::Busy => {
                    return ReedlineEvent::ExecuteHostCommand(BUSY_STEER_COMMAND.to_string());
                }
                Intercept::Background => {
                    return ReedlineEvent::ExecuteHostCommand(BACKGROUND_COMMAND.to_string());
                }
                Intercept::Interrupt if self.phase.get() == EditPhase::Busy => {
                    return ReedlineEvent::ExecuteHostCommand(INTERRUPT_COMMAND.to_string());
                }
                Intercept::Editor | Intercept::SteerBusy | Intercept::Interrupt => {}
            }
        }

        match ReedlineRawEvent::try_from(raw) {
            Ok(event) => self.editor.parse_event(event),
            // Unreachable in practice: the only rejected event is a key
            // release, and `raw` came out of a `ReedlineRawEvent`, which is
            // the type that already excluded one.
            Err(()) => ReedlineEvent::None,
        }
    }

    fn edit_mode(&self) -> PromptEditMode {
        if self.phase.get() == EditPhase::Modal {
            self.modal_editor.edit_mode()
        } else {
            self.editor.edit_mode()
        }
    }
}

fn dismisses_notice(event: &Event) -> bool {
    match event {
        Event::Paste(_) => true,
        Event::Key(key)
            if matches!(
                key.code,
                KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
            ) =>
        {
            key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
        }
        _ => false,
    }
}

/// The one command the picker's keys travel under; see `SurfaceStatus::picker_keys`.
pub(super) const PICKER_KEYS: &str = "keys";

fn picker_command(command: &str) -> ReedlineEvent {
    ReedlineEvent::ExecuteHostCommand(format!("{MODEL_PICKER_COMMAND}{command}"))
}

fn question_selector_command(command: &str) -> ReedlineEvent {
    ReedlineEvent::ExecuteHostCommand(format!("{QUESTION_SELECTOR_COMMAND}{command}"))
}

fn permission_decision_command(command: &str) -> ReedlineEvent {
    ReedlineEvent::ExecuteHostCommand(format!("{PERMISSION_DECISION_COMMAND}{command}"))
}
