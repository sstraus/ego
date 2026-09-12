use super::*;
use unicode_width::UnicodeWidthStr;

pub(super) fn run_model_picker(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    picker: Arc<Mutex<ModelPicker>>,
    busy: bool,
    notice: Option<String>,
) -> Option<Line> {
    let after = if busy {
        EditPhase::Busy
    } else {
        EditPhase::Idle
    };
    let saved = enter_modal(editor, phase, status);
    let previous_picker = status
        .lock()
        .expect("surface status lock")
        .model_picker
        .replace(Arc::clone(&picker));
    if notice.is_some() {
        status.lock().expect("surface status lock").notice = notice;
        repaint_status(status);
    }
    let result = loop {
        let view = picker.lock().expect("model picker lock").view();
        let width = status.lock().expect("surface status lock").width;
        let prompt = Question::new(model_picker_prompt(&view, width), Arc::clone(status));
        let line = answer(false, editor.read_line(&prompt));
        let input = match &line {
            Line::HostCommand(HostCommand::ModelPicker(command)) => model_picker_input(command),
            Line::HostCommand(_) => None,
            Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => Some(PickerInput::Escape),
            Line::Blank | Line::Submitted(_) | Line::Refused(_) => None,
        };
        let Some(input) = input else {
            continue;
        };
        let outcome = picker.lock().expect("model picker lock").input(input);
        match outcome {
            Ok(PickerInputOutcome::Closed(outcome)) => match outcome {
                PickerOutcome::Cancelled => {
                    break matches!(&line, Line::Ended | Line::ExternalBreak(_)).then_some(line);
                }
                PickerOutcome::ModelSwitch(request) => {
                    break Some(Line::HostCommand(HostCommand::ModelSwitch(request)));
                }
            },
            Ok(PickerInputOutcome::Consumed | PickerInputOutcome::FocusChanged(_)) => {
                status.lock().expect("surface status lock").notice = None;
                repaint_status(status);
            }
            Err(error) => {
                status.lock().expect("surface status lock").notice = Some(error.to_string());
                repaint_status(status);
            }
        }
    };
    leave_modal(editor, phase, status, saved, after);
    status.lock().expect("surface status lock").model_picker = previous_picker;
    result
}

fn model_picker_input(command: &str) -> Option<PickerInput> {
    match command {
        "up" => Some(PickerInput::Up),
        "down" => Some(PickerInput::Down),
        "enter" => Some(PickerInput::Enter),
        "session" => Some(PickerInput::SessionOnly),
        "escape" => Some(PickerInput::Escape),
        number => number
            .strip_prefix("number:")?
            .parse()
            .ok()
            .map(PickerInput::Number),
    }
}

fn model_picker_prompt(view: &ModelPickerView, width: usize) -> String {
    let width = width.max(40);
    let mut rows = Vec::new();
    match view.focus {
        PickerFocus::Model => {
            rows.push("Select model".to_string());
            rows.push("✓ available; ! unavailable".to_string());
            rows.extend(view.models.iter().enumerate().map(|(index, row)| {
                let id = row.slug.to_string();
                let marker = if view.current_id.as_deref() == Some(id.as_str()) {
                    ">"
                } else {
                    " "
                };
                model_row(
                    marker,
                    index + 1,
                    &id,
                    availability_symbol(&row.availability),
                    width,
                )
            }));
            rows.extend(wrap_instruction(
                "Up/Down or number selects. Enter continues; Esc cancels.",
                width,
            ));
        }
        PickerFocus::Reasoning => {
            rows.extend(wrap_instruction(
                &format!(
                    "Select reasoning for {}",
                    view.selected_model
                        .as_deref()
                        .unwrap_or("the selected model")
                ),
                width,
            ));
            rows.extend(view.reasoning.iter().enumerate().map(|(index, reasoning)| {
                let marker = if view.current_id.as_deref() == Some(reasoning.as_str()) {
                    ">"
                } else {
                    " "
                };
                format!("{marker} {}. {reasoning:?}", index + 1)
            }));
            rows.extend(wrap_instruction(
                if view.persistent_only {
                    "Up/Down or number selects. Enter saves the default; Esc cancels."
                } else {
                    "Up/Down or number selects. Enter saves defaults; s uses this session; Esc cancels."
                },
                width,
            ));
        }
    }
    rows.join("\n")
}

fn availability_symbol(availability: &ego_ai::catalogue::ModelAvailability) -> &'static str {
    match availability {
        ego_ai::catalogue::ModelAvailability::Available => "✓",
        ego_ai::catalogue::ModelAvailability::Unavailable { .. } => "!",
    }
}

fn model_row(marker: &str, index: usize, slug: &str, availability: &str, width: usize) -> String {
    let prefix = format!("{marker} {index}. ");
    let suffix = format!(" | {availability}");
    let slug_width = width.saturating_sub(prefix.width() + suffix.width());
    format!("{prefix}{}{suffix}", truncate_cells(slug, slug_width))
}

fn truncate_cells(text: &str, width: usize) -> String {
    let line = crate::render::Line::plain(text);
    if line.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", line.truncate_to(width - 1).text())
}

fn wrap_instruction(instruction: &str, width: usize) -> Vec<String> {
    crate::render::Line::plain(instruction)
        .wrap(width.max(1))
        .into_iter()
        .map(|line| line.text())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_picker::ModelPickerRow;
    use crate::run::InteractiveSeat;
    use ego_ai::catalogue::{ModelAvailability, ModelAvailabilityCause, ModelSlug};
    use ego_core::generator::capabilities::ReasoningEffortSupport;
    use ego_core::generator::types::ThinkingLevel;
    use unicode_width::UnicodeWidthStr;

    fn view() -> ModelPickerView {
        ModelPickerView {
            open: true,
            focus: PickerFocus::Model,
            current_id: Some("openai-codex/gpt-5.6-sol".to_owned()),
            selected_model: None,
            models: vec![
                ModelPickerRow {
                    slug: ModelSlug::parse("openai-codex/gpt-5.6-sol").unwrap(),
                    max_context_tokens: Some(272_000),
                    reasoning: ReasoningEffortSupport::Any,
                    availability: ModelAvailability::Available,
                },
                ModelPickerRow {
                    slug: ModelSlug::parse("anthropic/claude-fable-5-1").unwrap(),
                    max_context_tokens: Some(200_000),
                    reasoning: ReasoningEffortSupport::Any,
                    availability: ModelAvailability::Unavailable {
                        cause: ModelAvailabilityCause::MissingCredential,
                        message: "set ANTHROPIC_API_KEY before choosing this model".to_owned(),
                    },
                },
            ],
            reasoning: Vec::new(),
            persistent_only: true,
        }
    }

    #[test]
    fn model_rows_are_width_bounded_and_keep_availability_progressive() {
        for width in [80, 60, 40] {
            let rendered = model_picker_prompt(&view(), width);
            assert!(
                rendered
                    .lines()
                    .all(|line| UnicodeWidthStr::width(line) <= width),
                "picker exceeded {width} cells:\n{rendered}"
            );
            assert!(rendered.contains("openai-codex/gpt-5.6-sol"));
            assert!(rendered.contains("unavailable"));
            assert!(!rendered.contains("MissingCredential"));
            assert!(!rendered.contains("ANTHROPIC_API_KEY"));
        }
    }
    #[test]
    fn reasoning_heading_and_controls_fit_narrow_terminals() {
        let mut view = view();
        view.focus = PickerFocus::Reasoning;
        view.selected_model = Some("anthropic/claude-haiku-4-5".to_owned());
        view.reasoning = vec![ThinkingLevel::Off];
        for width in [80, 60, 40] {
            let rendered = model_picker_prompt(&view, width);
            assert!(rendered.lines().all(|line| line.width() <= width));
            assert!(rendered.contains("anthropic/claude-haiku-4-5"));
        }
    }
    #[test]
    fn coordinator_idle_preserves_the_terminal_owners_modal_phase() {
        let phase = SurfacePhase::new(EditPhase::Modal);
        let (_input_sender, input) = bounded(CHANNEL_CAPACITY);
        let (control, _control_receiver) = bounded(CHANNEL_CAPACITY);
        let (_ingress, coordinator) = interactive_control();
        let registry = Arc::new(ActionRegistry::final_surface());
        let seat = Seat {
            kind: SeatKind::Live(LiveSeat {
                input,
                control,
                activated: AtomicBool::new(false),
                break_signal: Arc::new(AtomicBool::new(false)),
                phase: phase.clone(),
                status: Arc::new(Mutex::new(SurfaceStatus::with_registry(Arc::clone(
                    &registry,
                )))),
                git_worker: Mutex::new(None),
                owner: Mutex::new(None),
            }),
            terminal: true,
            registry,
            coordinator: Mutex::new(Some(coordinator)),
        };
        seat.idle();
        assert_eq!(phase.get(), EditPhase::Modal);
        // Once the terminal owner leaves the modal, ordinary activity settles.
        phase.set(EditPhase::Busy);
        seat.idle();
        assert_eq!(phase.get(), EditPhase::Idle);
        seat.idle();
        assert_eq!(phase.get(), EditPhase::Idle);
    }
}
