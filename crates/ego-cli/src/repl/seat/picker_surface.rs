use super::*;
use crate::render::{Capabilities, Depth, Style};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy)]
pub(super) enum PickerRole {
    Question,
    Selected,
    Label,
    Description,
    Hint,
}

/// One role table for question, model/reasoning and session picker frames.
pub(super) fn picker_row(text: impl Into<String>, role: PickerRole, depth: Depth) -> String {
    let style = match role {
        PickerRole::Question | PickerRole::Label => Style::PLAIN,
        PickerRole::Selected => Style::PLAIN.reverse().bold(),
        PickerRole::Description | PickerRole::Hint => Style::PLAIN.dim(),
    };
    crate::render::Line::styled(text, style).render(depth)
}

pub(super) fn picker_capabilities(status: &SharedStatus) -> Capabilities {
    let status = status.lock().expect("surface status lock");
    let mut caps = status.permission_capabilities.with_current_width();
    caps.width = picker_width(
        status.width,
        if caps.on_screen() {
            Some(caps.width)
        } else {
            crossterm::terminal::size()
                .ok()
                .map(|(cols, _)| usize::from(cols))
        },
    );
    caps
}

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
    // The frame is drawn from this row down. Read it once: later frames start at
    // the same row because the picker never lets one scroll the screen.
    let start_row = crossterm::cursor::position()
        .ok()
        .map(|(column, row)| usize::from(row) + usize::from(column > 0));
    let result = loop {
        let view = picker.lock().expect("model picker lock").view();
        let caps = picker_capabilities(status);
        let terminal = crossterm::terminal::size().ok();
        let width = caps.width;
        let height = picker_height(
            terminal.map_or(24, |(_, height)| usize::from(height)),
            start_row,
        );
        let prompt = Question::new(
            model_picker_prompt(&view, width, height, caps.depth),
            Arc::clone(status),
        );
        let line = answer(false, editor.read_line(&prompt));
        let _ = editor.erase_last_prompt();
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

/// Live geometry wins; the recorded status width is only a fallback.
fn picker_width(status_width: usize, terminal: Option<usize>) -> usize {
    terminal.unwrap_or(status_width)
}

/// Rows a frame needs to show its headings, three choices and a hint.
const MIN_FRAME_ROWS: usize = 8;

/// The rows the picker frame may use. The frame is drawn from `start_row` down,
/// and reedline starts a new prompt instead of reusing the old one when the
/// cursor ends on the bottom row, which leaves every earlier frame on screen. A
/// frame sized to the whole terminal ends there once the rows above it (banner,
/// status, composer) are counted. When fewer than [`MIN_FRAME_ROWS`] remain
/// below `start_row` the terminal scrolls whatever the size, so the whole
/// terminal is used.
fn picker_height(terminal_rows: usize, start_row: Option<usize>) -> usize {
    match start_row.map(|row| terminal_rows.saturating_sub(row)) {
        Some(below) if below >= MIN_FRAME_ROWS => below,
        _ => terminal_rows,
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

fn model_picker_prompt(
    view: &ModelPickerView,
    width: usize,
    height: usize,
    depth: Depth,
) -> String {
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
                    "Enter continues; Esc cancels.",
                )
            };
            // A short terminal keeps its rows and gives up the typing hint.
            let mut instructions = wrap_instruction(full, width);
            if height.saturating_sub(3 + instructions.len()) < MIN_VISIBLE_MODELS {
                instructions = wrap_instruction(compact, width);
            }
            let visible_rows = height.saturating_sub(2 + instructions.len()).max(1);
            let (models, start, end) = model_window(view, width, visible_rows, depth);
            let title = format!(
                "Select model ({}-{end} of {})",
                if end == 0 { 0 } else { start + 1 },
                view.models.len()
            );
            rows.push(picker_row(
                if view.filter.is_empty() {
                    title
                } else {
                    truncate_cells(&format!("{title} · filter: {}", view.filter), width)
                },
                PickerRole::Question,
                depth,
            ));
            if view.models.is_empty() {
                rows.push(truncate_cells(
                    &format!("No model matches \"{}\"; Backspace edits.", view.filter),
                    width,
                ));
            }
            rows.extend(models);
            rows.extend(
                instructions
                    .into_iter()
                    .map(|row| picker_row(row, PickerRole::Hint, depth)),
            );
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
            rows.extend(
                heading
                    .into_iter()
                    .map(|row| picker_row(row, PickerRole::Question, depth)),
            );
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
                    picker_row(
                        format!("{marker} {}. {reasoning:?}{current}", index + 1),
                        if marker == ">" {
                            PickerRole::Selected
                        } else {
                            PickerRole::Label
                        },
                        depth,
                    )
                },
            ));
            rows.extend(
                instructions
                    .into_iter()
                    .map(|row| picker_row(row, PickerRole::Hint, depth)),
            );
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

/// Window physical rows, retaining the provider explanation when its first
/// model has scrolled out of view. Headings are not selectable model numbers.
fn model_window(
    view: &ModelPickerView,
    width: usize,
    visible: usize,
    depth: Depth,
) -> (Vec<String>, usize, usize) {
    let mut lines = Vec::new();
    let mut groups = Vec::new();
    let mut indices = Vec::new();
    let mut selected = None;
    let mut previous = None;
    let mut heading = Vec::new();
    for (index, row) in view.models.iter().enumerate() {
        let group = match &row.availability {
            ego_ai::catalogue::ModelAvailability::Available => None,
            ego_ai::catalogue::ModelAvailability::Unavailable { cause, .. } => {
                Some((row.slug.provider.as_str(), *cause))
            }
        };
        if group != previous {
            heading = group.map_or_else(Vec::new, |(provider, cause)| {
                provider_unavailable(provider, cause)
                    .lines()
                    .flat_map(|line| wrap_instruction(line, width))
                    .collect()
            });
            for (part, text) in heading.iter().enumerate() {
                lines.push(text.clone());
                groups.push(heading[..part].to_vec());
                indices.push(None);
            }
            previous = group;
        }
        let id = row.slug.to_string();
        let marker = if view.current_id.as_deref() == Some(id.as_str()) {
            selected = Some(lines.len());
            ">"
        } else {
            " "
        };
        let suffix = if view.current_model.as_deref() == Some(id.as_str()) {
            " current"
        } else {
            ""
        };
        let text = model_row(marker, index + 1, &id, suffix, width);
        lines.push(picker_row(
            text,
            if marker == ">" {
                PickerRole::Selected
            } else if group.is_some() {
                PickerRole::Description
            } else {
                PickerRole::Label
            },
            depth,
        ));
        groups.push(heading.clone());
        indices.push(Some(index));
    }
    let (mut start, _) = window_bounds(lines.len(), visible, selected);
    // Do not put a continuation heading between its selected model and the
    // viewport. At narrow sizes, prefer the model over unrelated earlier rows.
    while selected.is_some_and(|selected| {
        selected >= start + visible.saturating_sub(groups[start].len()).max(1)
    }) {
        start += 1;
    }
    let context = groups.get(start).cloned().unwrap_or_default();
    let mut end = (start + visible.saturating_sub(context.len()).max(1)).min(lines.len());
    while end > start && indices[end - 1].is_none() {
        end -= 1;
    }
    let first = indices[start..end]
        .iter()
        .flatten()
        .next()
        .copied()
        .unwrap_or(0);
    let last = indices[start..end]
        .iter()
        .flatten()
        .last()
        .map_or(0, |index| index + 1);
    let mut rendered = context;
    rendered.extend_from_slice(&lines[start..end]);
    (rendered, first, last)
}

fn provider_unavailable(
    provider: &str,
    cause: ego_ai::catalogue::ModelAvailabilityCause,
) -> String {
    use ego_ai::catalogue::ModelAvailabilityCause;
    let remedy = match cause {
        ModelAvailabilityCause::MissingCredential => {
            format!("not signed in\nRun `ego auth login {provider}`")
        }
        ModelAvailabilityCause::SourceUnreachable => {
            format!(
                "cannot reach provider; check its connection, then run `ego models --provider {provider} --refresh`"
            )
        }
        ModelAvailabilityCause::SourceMalformed | ModelAvailabilityCause::IncompleteFacts => {
            format!(
                "model details missing or invalid; run `ego models --provider {provider} --refresh`"
            )
        }
        ModelAvailabilityCause::MappingUnavailable => {
            "provider not supported; choose another provider".to_owned()
        }
    };
    format!("{provider} — unavailable: {remedy}")
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

    fn model_picker_prompt(view: &ModelPickerView, width: usize, height: usize) -> String {
        super::model_picker_prompt(view, width, height, Depth::TrueColor)
    }

    fn plain_picker(rendered: &str) -> String {
        let mut plain = String::new();
        let mut escape = false;
        for ch in rendered.chars() {
            if ch == '\x1b' {
                escape = true;
            } else if escape {
                if ch == 'm' {
                    escape = false;
                }
            } else {
                plain.push(ch);
            }
        }
        plain
    }

    // Catches: scrolled model rows depend on a legend for a bare pipe and symbol.
    #[test]
    fn scrolled_picker_rows_have_no_bare_separator_or_symbol() {
        let mut view = view();
        for index in 0..30 {
            let mut missing = view.models[1].clone();
            missing.slug.model = format!("model-{index:02}");
            view.models.push(missing);
        }
        view.current_id = Some(view.models[25].slug.to_string());
        for width in [40, 60, 80] {
            let rendered = model_picker_prompt(&view, width, 12);
            assert!(!rendered.contains('|'), "{rendered}");
            assert!(!rendered.contains('!'), "{rendered}");
            assert!(!rendered.contains("✓ available"), "{rendered}");
            assert!(
                plain_picker(&rendered)
                    .lines()
                    .all(|line| line.width() <= width)
            );
        }
    }

    // Catches: unavailable provider rows omit the reason and recovery command.
    #[test]
    fn unavailable_provider_group_explains_credentials_without_secret_details() {
        let mut view = view();
        view.models.push(view.models[1].clone());
        let rendered = model_picker_prompt(&view, 80, 24);
        assert_eq!(
            rendered.matches("anthropic — unavailable").count(),
            1,
            "{rendered}"
        );
        assert!(rendered.contains("not signed in"), "{rendered}");
        assert!(rendered.contains("ego auth login anthropic"), "{rendered}");
        assert!(!rendered.contains("MissingCredential"));
        assert!(!rendered.contains("ANTHROPIC_API_KEY"));
    }

    // Catches: scrolling inside an unavailable group loses its recovery context.
    #[test]
    fn scrolled_provider_keeps_its_heading_and_selected_model_at_every_width() {
        let mut view = view();
        for index in 0..30 {
            let mut row = view.models[1].clone();
            row.slug.model = format!("model-{index:02}");
            view.models.push(row);
        }
        for width in [40, 60, 80] {
            for index in [0, 1, 16, 31] {
                view.current_id = Some(view.models[index].slug.to_string());
                let rendered = model_picker_prompt(&view, width, 12);
                let plain = plain_picker(&rendered);
                assert!(plain.lines().count() < 12, "{plain}");
                assert!(plain.lines().all(|line| line.width() <= width), "{plain}");
                assert!(plain.contains(&format!("> {}.", index + 1)), "{plain}");
                if index > 0 {
                    assert!(plain.contains("anthropic — unavailable"), "{plain}");
                    assert!(plain.contains("ego auth login anthropic"), "{plain}");
                    assert!(rendered.contains("\x1b[2m"), "models must be dimmed");
                }
            }
        }
    }

    // Catches: discovery failures misleadingly recommend signing in.
    #[test]
    fn provider_failures_name_their_own_recovery_not_credentials() {
        for cause in [
            ModelAvailabilityCause::SourceUnreachable,
            ModelAvailabilityCause::SourceMalformed,
            ModelAvailabilityCause::IncompleteFacts,
            ModelAvailabilityCause::MappingUnavailable,
        ] {
            let mut view = view();
            view.models.retain(|row| row.slug.provider == "anthropic");
            view.models[0].slug.provider = "ollama".to_owned();
            view.models[0].availability = ModelAvailability::Unavailable {
                cause,
                message: "private discovery details".to_owned(),
            };
            let text = model_picker_prompt(&view, 120, 24);
            assert!(text.contains("ollama — unavailable"), "{text}");
            assert!(!text.contains("private discovery details"), "{text}");
            assert!(!text.contains("login"));
            assert!(!text.contains(&format!("{cause:?}")));
            assert!(
                text.contains(if cause == ModelAvailabilityCause::MappingUnavailable {
                    "choose another provider"
                } else {
                    "ego models --provider ollama --refresh"
                })
            );
        }
    }

    #[test]
    fn a_picker_before_session_setup_draws_in_the_terminal_width() {
        assert_eq!(picker_width(0, Some(120)), 120);
        assert_eq!(picker_width(100, Some(120)), 120);
        assert_eq!(picker_width(0, None), 0);
    }

    #[test]
    fn model_rows_are_width_bounded_and_keep_availability_progressive() {
        for width in [80, 60, 40] {
            let rendered = model_picker_prompt(&view(), width, 24);
            assert!(
                plain_picker(&rendered)
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
    /// Catches: a frame sized to the whole terminal while rows above it are
    /// already used, which ends the frame on the bottom row.
    #[test]
    fn the_frame_stops_above_the_bottom_row_of_the_terminal() {
        assert_eq!(picker_height(40, Some(9)), 31);
        assert_eq!(picker_height(40, Some(0)), 40);
        assert_eq!(picker_height(40, None), 40);
        // Too little room below the start row: the terminal scrolls anyway.
        assert_eq!(picker_height(8, Some(7)), 8);
        assert_eq!(picker_height(24, Some(23)), 24);
        let mut view = view();
        let row = view.models[0].clone();
        view.models = std::iter::repeat_n(row, 40).collect();
        let rendered = model_picker_prompt(&view, 80, picker_height(40, Some(9)));
        assert!(
            9 + rendered.lines().count() < 40,
            "the frame ends on the bottom row:\n{rendered}"
        );
    }
    #[test]
    fn reasoning_heading_and_controls_fit_narrow_terminals() {
        let mut view = view();
        view.focus = PickerFocus::Reasoning;
        view.selected_model = Some("anthropic/claude-haiku-4-5".to_owned());
        view.reasoning = vec![ThinkingLevel::Off];
        for width in [80, 60, 40] {
            let rendered = model_picker_prompt(&view, width, 24);
            assert!(
                plain_picker(&rendered)
                    .lines()
                    .all(|line| line.width() <= width)
            );
            assert!(rendered.contains("anthropic/claude-haiku-4-5"));
        }
    }
    #[test]
    fn coordinator_idle_preserves_the_terminal_owners_modal_phase() {
        let phase = SurfacePhase::new(EditPhase::Modal);
        let (_inspection_sender, inspections) = crossbeam::channel::unbounded();
        let (_input_sender, input) = bounded(CHANNEL_CAPACITY);
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
