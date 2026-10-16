//! The interactive terminal surface.
//!
//! One dedicated thread owns Reedline for the lifetime of a terminal session.
//! Rendered rows enter through Reedline's bounded external printer, so editing
//! and transcript output stay live without creating a second terminal writer.

mod clipboard;
mod keys;
mod seat;

pub mod actions;
pub mod policy_controls;
pub mod selector;
pub mod status;
pub mod surface;

/// What Esc leaves in the transcript, whether it stopped a turn or answered a
/// permission modal.
pub(crate) const INTERRUPTED_RECEIPT: &str = "Interrupted by user";

pub(crate) use policy_controls::PermissionRuleApplication;
pub(crate) use seat::Confirmation;
pub(crate) use seat::effective_statusline_segments;
pub use seat::{
    DraftStash, EditPhase, Seat, StatusSetup, SurfaceEditMode, SurfaceOutput, SurfacePhase,
};
pub(crate) use seat::{render_action_help, render_key_hints};

use actions::{ActionId, ExportSelection, SkillSurfaceAction};
use ego_core::skill::AdmittedSkillOperation;
use policy_controls::PolicyControlRequest;

use reedline::Signal;

pub(super) const DRAFT_COMMAND: &str = "ego:draft";
pub(super) const BUSY_SUBMIT_COMMAND: &str = "ego:busy-submit";
pub(super) const BUSY_RECALL_COMMAND: &str = "ego:busy-recall";
pub(super) const BUSY_STEER_COMMAND: &str = "ego:busy-steer";
pub(super) const BACKGROUND_COMMAND: &str = "ego:background";
const INTERRUPT_COMMAND: &str = "ego:interrupt";
pub(super) const PASTE_PATH_COMMAND: &str = "ego:paste-path:";
pub(super) const REMOVE_PASTE_IMAGE_COMMAND: &str = "ego:remove-paste-image";
pub(super) const CLIPBOARD_PASTE_COMMAND: &str = "ego:clipboard-paste";

/// A non-prompt editor event addressed to the interactive host.
///
/// Reedline's extension point carries strings, so the terminal adapter parses
/// that foreign representation exactly once. Everything beyond this boundary
/// receives this owned vocabulary and cannot mistake a control for user text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCommand {
    Action(ActionId),
    History(String),
    Resume(Option<String>),
    Fork(ego_core::event::id::ItemId),
    SessionMetadata(crate::cli::SessionMetadataOperation),
    Policy(PolicyControlRequest),
    Skill(AdmittedSkillOperation),
    Skills(SkillSurfaceAction),
    /// `/export`, carrying what it was asked for. Not an
    /// [`ActionId::Export`](actions::ActionId::Export) any more: an action id
    /// alone cannot say `json`, and the shortcut and the menu reach the same
    /// handler by sending [`ExportSelection::default`].
    Export(ExportSelection),
    Hooks,
    Memory(String),
    Init(String),
    Config(String),
    Setup(String),
    Tasks(String),
    Mcp(String),
    Background,
    ModelSwitch(crate::model_picker::ModelSwitchRequest),
    Draft,
    BusySubmit,
    BusySteer,
    BusyRecall,
    Interrupt,
    ModalEscape,
    ModelPicker(String),
    QuestionSelector(String),
    PermissionDecision(String),
    PastePath(String),
    RemovePasteImage,
    ClipboardPaste,
}

impl HostCommand {
    fn from_editor(command: String) -> Option<Self> {
        if command == DRAFT_COMMAND {
            return Some(Self::Draft);
        }
        if command == BUSY_SUBMIT_COMMAND {
            return Some(Self::BusySubmit);
        }
        if command == BUSY_RECALL_COMMAND {
            return Some(Self::BusyRecall);
        }
        if command == BUSY_STEER_COMMAND {
            return Some(Self::BusySteer);
        }
        if command == BACKGROUND_COMMAND {
            return Some(Self::Background);
        }
        if command == INTERRUPT_COMMAND {
            return Some(Self::Interrupt);
        }
        if command == "permission" {
            return Some(Self::Action(ActionId::Permissions));
        }
        if command == seat::MODAL_ESCAPE_COMMAND {
            return Some(Self::ModalEscape);
        }
        if let Some(path) = command.strip_prefix(PASTE_PATH_COMMAND) {
            return Some(Self::PastePath(path.to_owned()));
        }
        if command == REMOVE_PASTE_IMAGE_COMMAND {
            return Some(Self::RemovePasteImage);
        }
        if command == CLIPBOARD_PASTE_COMMAND {
            return Some(Self::ClipboardPaste);
        }
        if let Some(name) = command.strip_prefix(seat::ACTION_COMMAND) {
            return actions::ActionRegistry::final_surface()
                .resolve_slash(name)
                .map(Self::Action);
        }
        if let Some(input) = command.strip_prefix(seat::MODEL_PICKER_COMMAND) {
            return Some(Self::ModelPicker(input.to_string()));
        }
        if let Some(input) = command.strip_prefix(seat::PERMISSION_DECISION_COMMAND) {
            return Some(Self::PermissionDecision(input.to_string()));
        }
        command
            .strip_prefix(seat::QUESTION_SELECTOR_COMMAND)
            .map(|input| Self::QuestionSelector(input.to_string()))
    }
}

/// What one read of the input line produced.
///
/// The editor's own vocabulary is turned into this before anything else looks
/// at it, so the loop's decisions are made over a type this crate owns and can
/// test without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A prompt to run. Bracketed paste is on, so this may hold newlines and
    /// still be one prompt.
    Submitted(String),
    /// Enter on a line with nothing on it. Not a turn: a turn costs money, and
    /// this says nothing to spend it on.
    Blank,
    /// `Ctrl+C`. Abandon the line, keep the session.
    Interrupted,
    /// `Ctrl+D` on an empty line, or the terminal went away.
    Ended,
    /// A key bound to something this build does not have, named.
    Refused(&'static str),
    /// An editor event addressed to the host, never submitted as prompt text.
    HostCommand(HostCommand),
    /// A host lifecycle wake carrying the exact composer buffer.
    ExternalBreak(String),
}

impl Line {
    /// Read the editor's answer as one of ours.
    ///
    /// [`Line::Refused`] is produced by the key interception path before this
    /// conversion. Host and lifecycle signals retain distinct owned variants,
    /// so neither can be mistaken for submitted input.
    pub fn of(signal: Signal) -> Self {
        match signal {
            Signal::Success(text) if text.trim().is_empty() => Self::Blank,
            Signal::Success(text) => Self::Submitted(text),
            Signal::CtrlC => Self::Interrupted,
            Signal::CtrlD => Self::Ended,
            Signal::HostCommand(command) => HostCommand::from_editor(command)
                .map(Self::HostCommand)
                .unwrap_or(Self::Ended),
            Signal::ExternalBreak(buffer) => Self::ExternalBreak(buffer),
            _ => Self::Ended,
        }
    }
}

/// Turn one read into a [`Line`], key first and buffer second.
///
/// The ordering is the refusal: `Ctrl+V` exits the read carrying whatever had
/// already been typed, so consulting the buffer first would submit a
/// half-written prompt as though Enter had been pressed.
fn answer(asked_for_image: bool, signal: std::io::Result<Signal>) -> Line {
    if asked_for_image {
        return Line::HostCommand(HostCommand::ClipboardPaste);
    }
    match signal {
        Ok(signal) => Line::of(signal),
        // A read that fails is a terminal that went away: there is no line to
        // retype and nobody left to retype it.
        Err(_) => Line::Ended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::policy_controls::{PolicyControlController, PolicyControlRequest};
    use ego_core::config::Mode;
    use ego_core::event::skill::{
        SkillContentDigest, SkillDescriptorEvidence, SkillId, SkillSourceAuthority,
        SkillSourceLocation,
    };

    #[test]
    fn a_pasted_stack_trace_is_one_prompt_and_not_its_first_line() {
        let pasted = "thread 'main' panicked at src/main.rs:4:5:\nindex out of bounds\nnote: run with RUST_BACKTRACE=1";

        let line = Line::of(Signal::Success(pasted.to_string()));

        assert_eq!(line, Line::Submitted(pasted.to_string()));
    }

    #[test]
    fn policy_controls_cross_the_host_boundary_as_one_typed_command() {
        let mut controller = PolicyControlController::new();
        let request = controller
            .select_mode(Mode::Plan)
            .expect("plan emits a typed policy request");
        assert_eq!(
            Line::HostCommand(HostCommand::Policy(request.clone())),
            Line::HostCommand(HostCommand::Policy(PolicyControlRequest::SetMode(
                Mode::Plan
            )))
        );
    }

    #[test]
    fn ctrl_c_abandons_the_line_and_keeps_the_session() {
        assert_eq!(Line::of(Signal::CtrlC), Line::Interrupted);
    }

    #[test]
    fn ctrl_d_ends_the_session() {
        assert_eq!(Line::of(Signal::CtrlD), Line::Ended);
    }

    /// Nothing is spent on a prompt that says nothing. A blank submission is a
    /// real event — the person pressed Enter — but it is not a turn, and a
    /// turn is what costs money.
    #[test]
    fn a_line_with_nothing_on_it_is_not_a_turn() {
        for blank in ["", "   ", "\t", "\n", "  \n\t "] {
            assert_eq!(
                Line::of(Signal::Success(blank.to_string())),
                Line::Blank,
                "{blank:?}"
            );
        }
    }

    /// The key is answered before the buffer is, and that ordering is the
    /// whole refusal. `Ctrl+V` exits the read with whatever had already been
    /// typed, so reading the buffer first would submit a half-written prompt
    /// as if the person had pressed Enter — spending money on a turn they
    /// never asked for.
    #[test]
    fn the_key_is_answered_before_whatever_was_already_typed() {
        let half_written = Ok(Signal::Success("explain this err".to_string()));

        match answer(true, half_written) {
            Line::HostCommand(HostCommand::ClipboardPaste) => {}
            other => panic!("Ctrl+V must request paste without submitting, not {other:?}"),
        }
    }

    #[test]
    fn without_the_key_the_buffer_is_just_a_prompt() {
        assert_eq!(
            answer(false, Ok(Signal::Success("hello".to_string()))),
            Line::Submitted("hello".to_string())
        );
    }

    /// A read that fails is a terminal that went away. There is no line to
    /// retype and no one to retype it, so it ends the session rather than
    /// looping on an error forever.
    #[test]
    fn a_terminal_that_went_away_ends_the_session() {
        let broken = Err(std::io::Error::other("the terminal went away"));

        assert_eq!(answer(false, broken), Line::Ended);
    }

    /// The one thing that must never be confusable with a refusal: a person
    /// typing text that *looks* like an internal signal is still just a
    /// prompt. reedline 0.44 returns a host command as `Signal::Success`, so
    /// if the refusal ever moved onto a sentinel string this test is where it
    /// would be caught.
    #[test]
    fn a_prompt_cannot_impersonate_a_key() {
        for typed in ["ego:paste-image", "Ctrl+V", "\\x16"] {
            assert_eq!(
                Line::of(Signal::Success(typed.to_string())),
                Line::Submitted(typed.to_string()),
                "{typed:?}"
            );
        }
    }

    #[test]
    fn host_command_carries_the_admitted_skill_operation_without_prompt_coercion() {
        let location = SkillSourceLocation::parse("skills/explain/SKILL.md").unwrap();
        let digest = SkillContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let descriptor = SkillDescriptorEvidence {
            id: SkillId::derive(SkillSourceAuthority::User, &location, "explain", &digest),
            name: "explain".to_owned(),
            description: "Explain evidence".to_owned(),
            trigger: None,
            argument_hint: None,
            source: SkillSourceAuthority::User,
            location,
            source_digest: digest,
            user_invocable: true,
            model_invocable: true,
        };
        let operation = AdmittedSkillOperation::invoke(&descriptor, "the exact raw tail");
        let line = Line::HostCommand(HostCommand::Skill(operation.clone()));

        assert_eq!(
            line,
            Line::HostCommand(HostCommand::Skill(operation.clone()))
        );
        assert_eq!(operation.name(), "explain");
        assert_eq!(operation.raw_arguments(), "the exact raw tail");
        assert_ne!(line, Line::Submitted("the exact raw tail".to_owned()));
    }
}

#[cfg(test)]
mod live_surface_red {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use reedline::{EditMode, Emacs, ReedlineEvent, ReedlineRawEvent};
    use std::sync::{Arc, atomic::AtomicBool, mpsc};
    use std::time::Duration;

    #[test]
    fn host_lifecycle_signals_have_owned_line_variants() {
        assert!(matches!(
            Line::of(Signal::HostCommand("permission".into())),
            Line::HostCommand(_)
        ));
        assert!(matches!(
            Line::of(Signal::ExternalBreak("draft".into())),
            Line::ExternalBreak(_)
        ));
    }

    #[test]
    fn permissions_action_enters_the_local_surface_without_a_prompt() {
        assert_eq!(
            HostCommand::from_editor("ego:action:permissions".to_owned()),
            Some(HostCommand::Action(ActionId::Permissions))
        );
    }

    #[test]
    fn immediate_permission_key_is_never_interpreted_as_prompt_text() {
        assert_eq!(
            HostCommand::from_editor(format!("{}allow", seat::PERMISSION_DECISION_COMMAND)),
            Some(HostCommand::PermissionDecision("allow".to_owned()))
        );
    }

    #[test]
    fn edit_mode_submits_busy_input_to_the_host_and_delegates_idle_and_characters() {
        let phase = SurfacePhase::new(EditPhase::Busy);
        let mut mode = SurfaceEditMode::new(Emacs::default(), phase.clone());
        let escape =
            ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
                .unwrap();
        assert_eq!(
            mode.parse_event(escape),
            ReedlineEvent::ExecuteHostCommand(INTERRUPT_COMMAND.to_string())
        );
        let enter = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert_eq!(
            mode.parse_event(enter),
            ReedlineEvent::ExecuteHostCommand(BUSY_SUBMIT_COMMAND.to_string())
        );
        phase.set(EditPhase::Idle);
        let enter = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert!(matches!(mode.parse_event(enter), ReedlineEvent::Enter));
        let character = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert!(!matches!(mode.parse_event(character), ReedlineEvent::None));
    }

    #[test]
    fn surface_output_frames_rows_and_blocks_on_capacity_until_drained() {
        let (sender, receiver) = crossbeam::channel::bounded(1);
        let cancel = ego_core::turn::CancelToken::new();
        let output = SurfaceOutput::new(sender, cancel.clone());
        let (done_tx, done_rx) = mpsc::sync_channel(0);
        std::thread::scope(|scope| {
            let (started_tx, started_rx) = mpsc::sync_channel(0);
            scope.spawn(move || {
                let mut producer = output;
                started_tx.send(()).unwrap();
                producer.write("first\nsecond").unwrap();
                producer.write("\nthird").unwrap();
                producer.finish().unwrap();
                done_tx.send(()).unwrap();
            });
            started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("producer did not start");
            assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
            assert_eq!(receiver.recv().unwrap(), "first\n");
            assert_eq!(receiver.recv().unwrap(), "second\n");
            assert_eq!(receiver.recv().unwrap(), "third");
            assert!(done_rx.recv_timeout(Duration::from_secs(1)).is_ok());
        });
        drop(receiver);
        let (sender, receiver) = crossbeam::channel::bounded(1);
        let mut disconnected = SurfaceOutput::new(sender, cancel.clone());
        drop(receiver);
        assert!(disconnected.write("after disconnect\n").is_err());
        assert_eq!(
            cancel.cause(),
            Some(ego_core::event::domain::CancelCause::Host)
        );
    }

    #[test]
    fn live_output_pump_keeps_generation_production_cancellable_and_finish_is_a_barrier() {
        let (external, rendered) = crossbeam::channel::bounded(1);
        let cancel = ego_core::turn::CancelToken::new();
        let (mut output, pump) =
            SurfaceOutput::pumped(external, cancel, Arc::new(AtomicBool::new(false)));

        output.write("first\nsecond\nthird").unwrap();

        let (done_tx, done_rx) = mpsc::sync_channel(0);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                output.finish().unwrap();
                done_tx.send(()).unwrap();
            });
            assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
            assert_eq!(rendered.recv().unwrap(), "first\n");
            assert_eq!(rendered.recv().unwrap(), "second\n");
            assert_eq!(rendered.recv().unwrap(), "third");
            assert_eq!(rendered.recv().unwrap(), "");
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        });
        drop(rendered);
        pump.join().expect("output pump thread panicked");
    }

    #[test]
    fn live_output_pump_failure_cancels_the_host() {
        let (external, rendered) = crossbeam::channel::bounded(1);
        drop(rendered);
        let cancel = ego_core::turn::CancelToken::new();
        let (mut output, pump) =
            SurfaceOutput::pumped(external, cancel.clone(), Arc::new(AtomicBool::new(false)));

        output.write("undeliverable").unwrap();
        assert!(output.finish().is_err());
        assert_eq!(
            cancel.cause(),
            Some(ego_core::event::domain::CancelCause::Host)
        );
        pump.join().expect("output pump thread panicked");
    }
}

#[cfg(test)]
pub(crate) use seat::test_resumed_round_terminal;
