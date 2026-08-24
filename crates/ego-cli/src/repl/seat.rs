//! The seat: one lifetime owner for the interactive terminal.

use std::borrow::Cow;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam::channel::{Receiver, SendError, Sender, bounded};
use crossterm::event::Event;
use reedline::{
    EditCommand, EditMode, Emacs, ExternalPrinter, Prompt, PromptEditMode, PromptHistorySearch,
    Reedline, ReedlineEvent, ReedlineRawEvent, default_emacs_keybindings,
};

use super::keys::{Intercept, intercept};
use super::{Line, answer};
use ego_core::config::Trust;
use ego_core::event::domain::CancelCause;
use ego_core::permit::{PermissionDecision, PermissionRequest};
use ego_core::turn::CancelToken;

const CHANNEL_CAPACITY: usize = 1;

/// Whether plain Enter may submit the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditPhase {
    Idle = 0,
    Busy = 1,
    Modal = 2,
}

/// Shared phase read directly by the editor thread's key mode.
#[derive(Debug, Clone)]
pub struct SurfacePhase(Arc<AtomicU8>);

impl SurfacePhase {
    pub fn new(phase: EditPhase) -> Self {
        Self(Arc::new(AtomicU8::new(phase as u8)))
    }

    pub fn set(&self, phase: EditPhase) {
        self.0.store(phase as u8, Ordering::Release);
    }

    fn get(&self) -> EditPhase {
        match self.0.load(Ordering::Acquire) {
            0 => EditPhase::Idle,
            2 => EditPhase::Modal,
            _ => EditPhase::Busy,
        }
    }
}

/// The bounded, row-framing producer handed to interactive renderers.
pub struct SurfaceOutput {
    sender: Sender<String>,
    pending: String,
    cancel: CancelToken,
}

impl SurfaceOutput {
    pub fn new(sender: Sender<String>, cancel: CancelToken) -> Self {
        Self {
            sender,
            pending: String::new(),
            cancel,
        }
    }

    pub fn write(&mut self, text: &str) -> Result<(), SendError<String>> {
        self.pending.push_str(text);
        while let Some(end) = self.pending.find('\n').map(|index| index + 1) {
            let row: String = self.pending.drain(..end).collect();
            self.send(row)?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), SendError<String>> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let row = std::mem::take(&mut self.pending);
        self.send(row)
    }

    fn send(&self, row: String) -> Result<(), SendError<String>> {
        self.sender.send(row).inspect_err(|_| {
            self.cancel.cancel(CancelCause::Host);
        })
    }
}

enum Control {
    Trust {
        workspace: String,
        reply: Sender<Trust>,
    },
    Permission {
        request: PermissionRequest,
        reply: Sender<PermissionDecision>,
    },
    Shutdown,
}

struct LiveSeat {
    input: Receiver<Line>,
    control: Sender<Control>,
    break_signal: Arc<AtomicBool>,
    phase: SurfacePhase,
    owner: Mutex<Option<std::thread::JoinHandle<()>>>,
}

struct SequentialSeat {
    editor: Mutex<Reedline>,
    asked_for_image: Arc<AtomicBool>,
}

enum SeatKind {
    Live(LiveSeat),
    Sequential(Box<SequentialSeat>),
}

/// The human input adapter. A real terminal uses the lifetime owner; piped
/// stdin retains the sequential adapter and its existing byte behavior.
pub struct Seat {
    kind: SeatKind,
    terminal: bool,
}

impl std::fmt::Debug for Seat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Seat")
    }
}

impl Seat {
    /// Build the applicable editor and, for a terminal, its only output port.
    ///
    /// Bracketed paste is not a nicety: without it a pasted stack trace
    /// submits its first line as a whole prompt, which is the failure that
    /// makes pasting one impossible. `CLI.md` §6.1.
    pub fn new(cancel: CancelToken) -> std::io::Result<(Self, Option<SurfaceOutput>)> {
        let terminal = std::io::stdin().is_terminal();
        let phase = SurfacePhase::new(if terminal {
            EditPhase::Busy
        } else {
            EditPhase::Idle
        });
        let mode = SurfaceEditMode::new(Emacs::new(default_emacs_keybindings()), phase.clone());
        let asked_for_image = mode.asked_for_image();
        let editor = Reedline::create()
            .with_edit_mode(Box::new(mode))
            .use_bracketed_paste(true);

        if !terminal {
            return Ok((
                Self {
                    kind: SeatKind::Sequential(Box::new(SequentialSeat {
                        editor: Mutex::new(editor),
                        asked_for_image,
                    })),
                    terminal,
                },
                None,
            ));
        }

        let printer = ExternalPrinter::new(CHANNEL_CAPACITY);
        let output = SurfaceOutput::new(printer.sender(), cancel.clone());
        let break_signal = Arc::new(AtomicBool::new(false));
        let (input_sender, input) = bounded(CHANNEL_CAPACITY);
        let (control, control_receiver) = bounded(CHANNEL_CAPACITY);
        let owner_break = Arc::clone(&break_signal);
        let owner_phase = phase.clone();
        let owner = std::thread::Builder::new()
            .name("ego-reedline".to_string())
            .spawn(move || {
                own_terminal(
                    editor
                        .with_external_printer(printer)
                        .with_break_signal(owner_break),
                    input_sender,
                    control_receiver,
                    owner_phase,
                    asked_for_image,
                    cancel,
                );
            })?;

        Ok((
            Self {
                kind: SeatKind::Live(LiveSeat {
                    input,
                    control,
                    break_signal,
                    phase,
                    owner: Mutex::new(Some(owner)),
                }),
                terminal,
            },
            Some(output),
        ))
    }

    /// Read one line.
    pub fn read(&self) -> Line {
        match &self.kind {
            SeatKind::Live(live) => {
                live.phase.set(EditPhase::Idle);
                live.input.recv().unwrap_or(Line::Ended)
            }
            SeatKind::Sequential(seat) => read_with(&seat.editor, &seat.asked_for_image, &Waiting),
        }
    }

    /// Publish the lifecycle reason before waking Reedline out of its read.
    pub fn shutdown(&self) {
        if let SeatKind::Live(live) = &self.kind {
            let _ = live.control.send(Control::Shutdown);
            live.break_signal.store(true, Ordering::Release);
        }
    }

    /// Join only after every external output producer has been dropped.
    pub fn join(&self) {
        let SeatKind::Live(live) = &self.kind else {
            return;
        };
        if let Some(owner) = live.owner.lock().expect("terminal owner lock").take() {
            let _ = owner.join();
        }
    }

    fn request<R>(&self, control: impl FnOnce(Sender<R>) -> Control, fallback: R) -> R {
        let SeatKind::Live(live) = &self.kind else {
            return fallback;
        };
        let (reply, answer) = bounded(0);
        if live.control.send(control(reply)).is_err() {
            return fallback;
        }
        live.break_signal.store(true, Ordering::Release);
        answer.recv().unwrap_or(fallback)
    }
}

fn read_with(editor: &Mutex<Reedline>, asked_for_image: &AtomicBool, prompt: &dyn Prompt) -> Line {
    let signal = editor.lock().expect("line editor lock").read_line(prompt);
    let asked_for_image = asked_for_image.swap(false, Ordering::Relaxed);
    answer(asked_for_image, signal)
}

impl crate::run::Reads for Seat {
    fn read(&self) -> Line {
        Seat::read(self)
    }
}

impl crate::run::InteractiveSeat for Seat {
    fn has_human(&self) -> bool {
        self.terminal
    }

    fn busy(&self) {
        if let SeatKind::Live(live) = &self.kind {
            live.phase.set(EditPhase::Busy);
        }
    }

    fn trust(&self, workspace: &Path) -> Trust {
        // Piped stdin may itself be the first prompt. Consuming it as a trust
        // answer would turn user content into authority. With no terminal
        // there is no human seat, so the only truthful answer is untrusted.
        if !self.terminal {
            return Trust::Untrusted;
        }
        self.request(
            |reply| Control::Trust {
                workspace: workspace.display().to_string(),
                reply,
            },
            Trust::Untrusted,
        )
    }

    fn permission(&self, request: &PermissionRequest) -> PermissionDecision {
        if !self.terminal {
            return PermissionDecision::Deny;
        }
        self.request(
            |reply| Control::Permission {
                request: request.clone(),
                reply,
            },
            PermissionDecision::Deny,
        )
    }
}

fn own_terminal(
    mut editor: Reedline,
    input: Sender<Line>,
    control: Receiver<Control>,
    phase: SurfacePhase,
    asked_for_image: Arc<AtomicBool>,
    cancel: CancelToken,
) {
    let mut pending_control = None;
    loop {
        if let Some(request) = pending_control.take() {
            if !answer_control(
                &mut editor,
                &phase,
                &asked_for_image,
                &control,
                &mut pending_control,
                request,
            ) {
                break;
            }
            continue;
        }

        let signal = editor.read_line(&Waiting);
        let failed = signal.is_err();
        if failed {
            cancel.cancel(CancelCause::Host);
        }
        let line = answer(asked_for_image.swap(false, Ordering::Relaxed), signal);
        match line {
            Line::ExternalBreak(_) => match control.recv() {
                Ok(Control::Shutdown) | Err(_) => break,
                Ok(request) => {
                    if !answer_control(
                        &mut editor,
                        &phase,
                        &asked_for_image,
                        &control,
                        &mut pending_control,
                        request,
                    ) {
                        break;
                    }
                }
            },
            Line::Submitted(_)
            | Line::Blank
            | Line::Interrupted
            | Line::Ended
            | Line::Refused(_)
            | Line::HostCommand(_) => {
                if matches!(line, Line::Submitted(_) | Line::Ended) {
                    phase.set(EditPhase::Busy);
                }
                let ended = matches!(line, Line::Ended);
                if input.send(line).is_err() || (ended && failed) {
                    break;
                }
            }
        }
    }
}

fn answer_control(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    asked_for_image: &AtomicBool,
    controls: &Receiver<Control>,
    pending_control: &mut Option<Control>,
    control: Control,
) -> bool {
    let saved = editor.current_buffer_contents().to_string();
    let cursor = editor.current_insertion_point();
    editor.run_edit_commands(&[EditCommand::Clear]);
    phase.set(EditPhase::Modal);

    enum Reply {
        Trust(Sender<Trust>, Trust),
        Permission(Sender<PermissionDecision>, PermissionDecision),
    }

    let (reply, after, mut keep_running) = match control {
        Control::Trust { workspace, reply } => {
            let prompt = Question(format!("Trust project files in {workspace}? [y/N] "));
            let trust = loop {
                match modal_line(editor, asked_for_image, &prompt) {
                    Line::Submitted(answer) => match parse_trust(&answer) {
                        Some(trust) => break trust,
                        None => continue,
                    },
                    Line::Blank | Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => {
                        break Trust::Untrusted;
                    }
                    Line::Refused(_) | Line::HostCommand(_) => continue,
                }
            };
            (Reply::Trust(reply, trust), EditPhase::Idle, true)
        }
        Control::Permission { request, reply } => {
            let prompt = Question(format!("{request}. [allow/deny/amend] "));
            let decision = loop {
                match modal_line(editor, asked_for_image, &prompt) {
                    Line::Submitted(answer) => match parse_permission(&answer) {
                        Some(PermissionWord::Allow) => break PermissionDecision::Allow,
                        Some(PermissionWord::Deny) => break PermissionDecision::Deny,
                        Some(PermissionWord::Amend) => break amendment(editor, asked_for_image),
                        None => continue,
                    },
                    Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => {
                        break PermissionDecision::Deny;
                    }
                    Line::Blank | Line::Refused(_) | Line::HostCommand(_) => continue,
                }
            };
            (Reply::Permission(reply, decision), EditPhase::Busy, true)
        }
        Control::Shutdown => return false,
    };
    match controls.try_recv() {
        Ok(Control::Shutdown) => keep_running = false,
        Ok(next) => *pending_control = Some(next),
        Err(crossbeam::channel::TryRecvError::Empty) => {}
        Err(crossbeam::channel::TryRecvError::Disconnected) => keep_running = false,
    }

    editor.run_edit_commands(&[
        EditCommand::Clear,
        EditCommand::InsertString(saved),
        EditCommand::MoveToPosition {
            position: cursor,
            select: false,
        },
    ]);
    phase.set(after);
    match reply {
        Reply::Trust(sender, trust) => {
            let _ = sender.send(trust);
        }
        Reply::Permission(sender, decision) => {
            let _ = sender.send(decision);
        }
    }
    keep_running
}

fn modal_line(editor: &mut Reedline, asked_for_image: &AtomicBool, prompt: &dyn Prompt) -> Line {
    answer(
        asked_for_image.swap(false, Ordering::Relaxed),
        editor.read_line(prompt),
    )
}

fn amendment(editor: &mut Reedline, asked_for_image: &AtomicBool) -> PermissionDecision {
    let prompt = Question("New JSON parameters: ".to_string());
    loop {
        match modal_line(editor, asked_for_image, &prompt) {
            Line::Submitted(raw) => match serde_json::from_str(&raw) {
                Ok(params) => return PermissionDecision::Amend { params },
                Err(_) => continue,
            },
            Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => {
                return PermissionDecision::Deny;
            }
            Line::Blank | Line::Refused(_) | Line::HostCommand(_) => continue,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionWord {
    Allow,
    Deny,
    Amend,
}

fn parse_trust(answer: &str) -> Option<Trust> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(Trust::Trusted),
        "n" | "no" => Some(Trust::Untrusted),
        _ => None,
    }
}

fn parse_permission(answer: &str) -> Option<PermissionWord> {
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
struct Waiting;

impl Prompt for Waiting {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
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
struct Question(String);

impl Prompt for Question {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed(&self.0)
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("… ")
    }

    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Owned(format!("(search: {}) ", search.term))
    }
}

/// The Emacs keymap with ego's phase and clipboard decisions taken first.
pub struct SurfaceEditMode {
    editor: Emacs,
    phase: SurfacePhase,
    asked_for_image: Arc<AtomicBool>,
}

impl SurfaceEditMode {
    pub fn new(editor: Emacs, phase: SurfacePhase) -> Self {
        Self {
            editor,
            phase,
            asked_for_image: Arc::new(AtomicBool::new(false)),
        }
    }

    fn asked_for_image(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.asked_for_image)
    }
}

impl EditMode for SurfaceEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let raw: Event = event.into();

        if let Event::Key(key) = raw {
            if intercept(key.code, key.modifiers) == Intercept::ClipboardImage {
                self.asked_for_image.store(true, Ordering::Relaxed);
                return ReedlineEvent::ExecuteHostCommand(String::new());
            }
            if key.code == reedline::KeyCode::Enter
                && key.modifiers.is_empty()
                && self.phase.get() == EditPhase::Busy
            {
                return ReedlineEvent::None;
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_plain_permission_vocabulary_includes_amend() {
        for (raw, expected) in [
            ("allow", PermissionWord::Allow),
            ("deny", PermissionWord::Deny),
            ("amend", PermissionWord::Amend),
            (" A ", PermissionWord::Allow),
            ("d", PermissionWord::Deny),
            ("m", PermissionWord::Amend),
        ] {
            assert_eq!(parse_permission(raw), Some(expected), "{raw:?}");
        }
        for invalid in ["", "yes", "no", "approve", "cancel"] {
            assert_eq!(parse_permission(invalid), None, "{invalid:?}");
        }
    }
}
