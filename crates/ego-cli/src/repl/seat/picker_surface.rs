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
        let status_width = status.lock().expect("surface status lock").width;
        let terminal = crossterm::terminal::size().ok();
        let width = picker_width(status_width, terminal.map(|(width, _)| usize::from(width)));
        let height = terminal.map_or(24, |(_, height)| usize::from(height));
        let prompt = Question::new(
            model_picker_prompt(&view, width, height),
            Arc::clone(status),
        );
        let line = answer(false, editor.read_line(&prompt));
        let inputs = match &line {
            Line::HostCommand(HostCommand::ModelPicker(_)) => {
                std::mem::take(&mut status.lock().expect("surface status lock").picker_keys)
                    .iter()
                    .filter_map(|command| model_picker_input(command))
                    .collect()
            }
            Line::HostCommand(_) => Vec::new(),
            Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => vec![PickerInput::Escape],
            Line::Blank | Line::Submitted(_) | Line::Refused(_) => Vec::new(),
        };
        let mut finished = None;
        for input in inputs {
            let outcome = picker.lock().expect("model picker lock").input(input);
            match outcome {
                Ok(PickerInputOutcome::Closed(outcome)) => {
                    finished = Some(match outcome {
                        PickerOutcome::Cancelled => {
                            matches!(&line, Line::Ended | Line::ExternalBreak(_))
                                .then(|| line.clone())
                        }
                        PickerOutcome::ModelSwitch(request) => {
                            Some(Line::HostCommand(HostCommand::ModelSwitch(request)))
                        }
                    });
                    break;
                }
                Ok(PickerInputOutcome::Consumed | PickerInputOutcome::FocusChanged(_)) => {
                    status.lock().expect("surface status lock").notice = None;
                    repaint_status(status);
                }
                Err(error) => {
                    status.lock().expect("surface status lock").notice = Some(error.to_string());
                    repaint_status(status);
                }
            }
        }
        if let Some(result) = finished {
            break result;
        }
    };
    leave_modal(editor, phase, status, saved, after);
    status.lock().expect("surface status lock").model_picker = previous_picker;
    result
}

/// The columns a picker draws in. The status width is only recorded once the
/// session is set up, so a picker that opens before that (the first-run one)
/// reads the terminal instead of drawing in the 40-column floor.
fn picker_width(status_width: usize, terminal: Option<usize>) -> usize {
    match status_width {
        0 => terminal.unwrap_or(0),
        width => width,
    }
}

fn model_picker_input(command: &str) -> Option<PickerInput> {
    match command {
        "up" => Some(PickerInput::Up),
        "down" => Some(PickerInput::Down),
        "enter" => Some(PickerInput::Enter),
        "session" => Some(PickerInput::SessionOnly),
        "escape" => Some(PickerInput::Escape),
        "backspace" => Some(PickerInput::Backspace),
        other => match other.strip_prefix("char:") {
            Some(typed) => typed.chars().next().map(PickerInput::Char),
            None => other
                .strip_prefix("number:")?
                .parse()
                .ok()
                .map(PickerInput::Number),
        },
    }
}

/// Model rows a terminal must still show before the typing hint is dropped.
const MIN_VISIBLE_MODELS: usize = 3;

fn model_picker_prompt(view: &ModelPickerView, width: usize, height: usize) -> String {
    let width = width.max(40);
    let mut rows = Vec::new();
    match view.focus {
        PickerFocus::Model => {
            let (full, compact) = if view.persistent_only {
                (
                    "Type to filter; Up/Down or number selects. Enter continues; Esc cancels.",
                    "Enter continues; Esc cancels.",
                )
            } else {
                (
                    "Type to filter; Up/Down or number selects. Enter continues, then s = this session only; Esc cancels.",
                    "Enter continues, then s = this session only; Esc cancels.",
                )
            };
            // A short terminal keeps its rows and gives up the typing hint.
            let mut instructions = wrap_instruction(full, width);
            if height.saturating_sub(3 + instructions.len()) < MIN_VISIBLE_MODELS {
                instructions = wrap_instruction(compact, width);
            }
            let visible_rows = height.saturating_sub(3 + instructions.len()).max(1);
            let selected = view
                .models
                .iter()
                .position(|row| view.current_id.as_deref() == Some(row.slug.to_string().as_str()));
            let (start, end) = window_bounds(view.models.len(), visible_rows, selected);
            let title = format!(
                "Select model ({}-{end} of {})",
                if end == 0 { 0 } else { start + 1 },
                view.models.len()
            );
            rows.push(if view.filter.is_empty() {
                title
            } else {
                truncate_cells(&format!("{title} · filter: {}", view.filter), width)
            });
            rows.push("✓ available; ! unavailable".to_string());
            if view.models.is_empty() {
                rows.push(truncate_cells(
                    &format!("No model matches \"{}\"; Backspace edits.", view.filter),
                    width,
                ));
            }
            rows.extend(
                view.models[start..end]
                    .iter()
                    .enumerate()
                    .map(|(offset, row)| {
                        let index = start + offset;
                        let id = row.slug.to_string();
                        let marker = if view.current_id.as_deref() == Some(id.as_str()) {
                            ">"
                        } else {
                            " "
                        };
                        let symbol = availability_symbol(&row.availability);
                        let suffix = if view.current_model.as_deref() == Some(id.as_str()) {
                            format!(" {symbol} current")
                        } else {
                            format!(" | {symbol}")
                        };
                        model_row(marker, index + 1, &id, &suffix, width)
                    }),
            );
            rows.extend(instructions);
        }
        PickerFocus::Reasoning => {
            let heading = wrap_instruction(
                &format!(
                    "Select reasoning for {}",
                    view.selected_model
                        .as_deref()
                        .unwrap_or("the selected model")
                ),
                width,
            );
            let instructions = wrap_instruction(
                if view.persistent_only {
                    "Up/Down or number selects. Enter saves the default; Esc cancels."
                } else {
                    "Up/Down or number selects. Enter saves defaults; s uses this session; Esc cancels."
                },
                width,
            );
            let visible_rows = height
                .saturating_sub(heading.len() + instructions.len() + 1)
                .max(1);
            let selected = view
                .reasoning
                .iter()
                .position(|reasoning| view.current_id.as_deref() == Some(reasoning.as_str()));
            let (start, end) = window_bounds(view.reasoning.len(), visible_rows, selected);
            rows.extend(heading);
            rows.extend(view.reasoning[start..end].iter().enumerate().map(
                |(offset, reasoning)| {
                    let index = start + offset;
                    let marker = if view.current_id.as_deref() == Some(reasoning.as_str()) {
                        ">"
                    } else {
                        " "
                    };
                    let current = if view.current_reasoning == Some(*reasoning) {
                        " ✓ current"
                    } else {
                        ""
                    };
                    format!("{marker} {}. {reasoning:?}{current}", index + 1)
                },
            ));
            rows.extend(instructions);
        }
    }
    rows.join("\n")
}

pub(super) fn window_bounds(
    total: usize,
    visible: usize,
    selected: Option<usize>,
) -> (usize, usize) {
    let visible = visible.min(total);
    let start = selected
        .unwrap_or(0)
        .saturating_sub(visible / 2)
        .min(total.saturating_sub(visible));
    (start, start + visible)
}

fn availability_symbol(availability: &ego_ai::catalogue::ModelAvailability) -> &'static str {
    match availability {
        ego_ai::catalogue::ModelAvailability::Available => "✓",
        ego_ai::catalogue::ModelAvailability::Unavailable { .. } => "!",
    }
}

fn model_row(marker: &str, index: usize, slug: &str, suffix: &str, width: usize) -> String {
    let prefix = format!("{marker} {index}. ");
    let slug_width = width.saturating_sub(prefix.width() + suffix.width());
    format!("{prefix}{}{suffix}", truncate_cells(slug, slug_width))
}

pub(super) fn truncate_cells(text: &str, width: usize) -> String {
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
            filter: String::new(),
            current_model: Some("openai-codex/gpt-5.6-sol".to_owned()),
            current_reasoning: None,
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
    fn a_picker_before_session_setup_draws_in_the_terminal_width() {
        assert_eq!(picker_width(0, Some(120)), 120);
        assert_eq!(picker_width(100, Some(120)), 100);
        assert_eq!(picker_width(0, None), 0);
    }

    #[test]
    fn model_rows_are_width_bounded_and_keep_availability_progressive() {
        for width in [80, 60, 40] {
            let rendered = model_picker_prompt(&view(), width, 24);
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
            let rendered = model_picker_prompt(&view, width, 24);
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
            kind: SeatKind::Live(Box::new(LiveSeat {
                input,
                control,
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
