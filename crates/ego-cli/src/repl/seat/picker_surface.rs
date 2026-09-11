use super::*;

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
    if notice.is_some() {
        status.lock().expect("surface status lock").notice = notice;
        repaint_status(status);
    }
    let result = loop {
        let view = picker.lock().expect("model picker lock").view();
        let prompt = Question::new(model_picker_prompt(&view), Arc::clone(status));
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

fn model_picker_prompt(view: &ModelPickerView) -> String {
    let mut rows = Vec::new();
    match view.focus {
        PickerFocus::Model => {
            rows.push("Select model".to_string());
            rows.extend(view.models.iter().enumerate().map(|(index, row)| {
                let id = row.slug.to_string();
                let marker = if view.current_id.as_deref() == Some(id.as_str()) {
                    ">"
                } else {
                    " "
                };
                let context = row
                    .max_context_tokens
                    .map(|tokens| tokens.to_string())
                    .unwrap_or_else(|| "unknown context".to_string());
                format!(
                    "{marker} {}. {} | {context} | {:?} | {:?}",
                    index + 1,
                    row.slug,
                    row.reasoning,
                    row.availability
                )
            }));
            rows.push("Up/Down or number selects; Enter continues; Esc cancels. ".to_string());
        }
        PickerFocus::Reasoning => {
            rows.push(format!(
                "Select reasoning for {}",
                view.selected_model
                    .as_deref()
                    .unwrap_or("the selected model")
            ));
            rows.extend(view.reasoning.iter().enumerate().map(|(index, reasoning)| {
                let marker = if view.current_id.as_deref() == Some(reasoning.as_str()) {
                    ">"
                } else {
                    " "
                };
                format!("{marker} {}. {reasoning:?}", index + 1)
            }));
            rows.push(if view.persistent_only {
                "Up/Down or number selects; Enter saves the default; Esc cancels. ".to_string()
            } else {
                "Up/Down or number selects; Enter saves defaults; s uses this session; Esc cancels. "
                    .to_string()
            });
        }
    }
    rows.join("\n")
}
