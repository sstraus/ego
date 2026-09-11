use super::*;

pub struct SurfaceEditMode {
    editor: Emacs,
    phase: SurfacePhase,
    status: SharedStatus,
    asked_for_image: Arc<AtomicBool>,
}

impl SurfaceEditMode {
    pub fn new(editor: Emacs, phase: SurfacePhase) -> Self {
        Self::with_status(
            editor,
            phase,
            Arc::new(Mutex::new(SurfaceStatus::with_registry(Arc::new(
                ActionRegistry::final_surface(),
            )))),
        )
    }

    pub(super) fn with_status(editor: Emacs, phase: SurfacePhase, status: SharedStatus) -> Self {
        Self {
            editor,
            phase,
            status,
            asked_for_image: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn asked_for_image(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.asked_for_image)
    }
}

impl EditMode for SurfaceEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let raw: Event = event.into();

        let picker_open = if self.phase.get() == EditPhase::Modal {
            let picker = self
                .status
                .lock()
                .expect("surface status lock")
                .model_picker
                .clone();
            picker.is_some_and(|picker| picker.lock().expect("model picker lock").is_open())
        } else {
            false
        };
        let question_selector_open = if self.phase.get() == EditPhase::Modal {
            self.status
                .lock()
                .expect("surface status lock")
                .question_selector
                .is_some()
        } else {
            false
        };

        match &raw {
            Event::Paste(_) if !picker_open && !question_selector_open => {
                self.status.lock().expect("surface status lock").origin =
                    InputOrigin::BracketedPaste;
            }
            Event::Resize(width, _) => {
                self.status.lock().expect("surface status lock").width = usize::from(*width);
                repaint_status(&self.status);
            }
            _ => {}
        }

        if picker_open {
            return match &raw {
                Event::Key(key) if key.modifiers.is_empty() => match key.code {
                    KeyCode::Up => picker_command("up"),
                    KeyCode::Down => picker_command("down"),
                    KeyCode::Enter => picker_command("enter"),
                    KeyCode::Esc => picker_command("escape"),
                    KeyCode::Char('s') => picker_command("session"),
                    KeyCode::Char(digit @ '1'..='9') => picker_command(&format!("number:{digit}")),
                    _ => ReedlineEvent::None,
                },
                Event::Key(key)
                    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL =>
                {
                    picker_command("escape")
                }
                Event::Resize(_, _) => ReedlineEvent::None,
                _ => ReedlineEvent::None,
            };
        }

        if question_selector_open {
            return match &raw {
                Event::Key(key) if key.modifiers.is_empty() => match key.code {
                    KeyCode::Up => question_selector_command("up"),
                    KeyCode::Down => question_selector_command("down"),
                    KeyCode::Enter => question_selector_command("enter"),
                    KeyCode::Esc => question_selector_command("escape"),
                    KeyCode::Char(' ') => question_selector_command("space"),
                    KeyCode::Char(digit @ '1'..='9') => {
                        question_selector_command(&format!("number:{digit}"))
                    }
                    _ => ReedlineEvent::None,
                },
                Event::Key(key)
                    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL =>
                {
                    question_selector_command("escape")
                }
                Event::Resize(_, _) => ReedlineEvent::None,
                _ => ReedlineEvent::None,
            };
        }

        if let Event::Key(key) = &raw {
            if self.phase.get() == EditPhase::Modal
                && key.code == reedline::KeyCode::Esc
                && key.modifiers.is_empty()
            {
                return ReedlineEvent::ExecuteHostCommand(MODAL_ESCAPE_COMMAND.to_string());
            }
            if self.phase.get() == EditPhase::Modal {
                return match ReedlineRawEvent::try_from(raw) {
                    Ok(event) => self.editor.parse_event(event),
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
            match intercept(key.code, key.modifiers) {
                Intercept::ClipboardImage => {
                    self.asked_for_image.store(true, Ordering::Relaxed);
                    return ReedlineEvent::ExecuteHostCommand(String::new());
                }
                Intercept::Draft => {
                    return ReedlineEvent::ExecuteHostCommand(DRAFT_COMMAND.to_string());
                }
                Intercept::SubmitBusy if self.phase.get() == EditPhase::Busy => {
                    return ReedlineEvent::ExecuteHostCommand(BUSY_SUBMIT_COMMAND.to_string());
                }
                Intercept::Interrupt
                    if self
                        .status
                        .lock()
                        .expect("surface status lock")
                        .queue
                        .pending_steers
                        > 0 =>
                {
                    return ReedlineEvent::ExecuteHostCommand(INTERRUPT_COMMAND.to_string());
                }
                Intercept::Editor | Intercept::SubmitBusy | Intercept::Interrupt => {}
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
        self.editor.edit_mode()
    }
}

fn picker_command(command: &str) -> ReedlineEvent {
    ReedlineEvent::ExecuteHostCommand(format!("{MODEL_PICKER_COMMAND}{command}"))
}

fn question_selector_command(command: &str) -> ReedlineEvent {
    ReedlineEvent::ExecuteHostCommand(format!("{QUESTION_SELECTOR_COMMAND}{command}"))
}
