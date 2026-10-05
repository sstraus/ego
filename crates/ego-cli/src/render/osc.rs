//! The out-of-band half: what ego tells the host that is not text on a screen.
//!
//! `ARCHITECTURE.md` §4 sets
//! the rule: an unknown OSC is discarded by every emulator in the matrix, so
//! the risk of emitting one is not garbage on screen — it is a capability
//! claimed and never delivered. Every sequence here appears in that matrix,
//! and the two that are not universal ([`Osc::notify`], [`Osc::tuic`]) are
//! chosen by [`OscProfile`] rather than emitted hopefully.
//!
//! ## Why every dynamic payload is sanitised
//!
//! An OSC string is terminated by BEL or by ST. The payloads below are not
//! ours: a tool name comes from a tool, an intent line comes from the model,
//! a notification body quotes an error a provider sent. A payload containing
//! `\x07` ends the sequence early and puts the rest of itself **on the
//! screen**, and a payload containing `\x1b` starts a new one — which is a
//! model deciding what escape sequences reach the terminal. That is the
//! injection this module exists to make impossible, and it is why every
//! dynamic field goes through [`sanitise`] rather than trusting its caller.
//! Fixed source literals, including the title prefix, are safe by construction
//! and are not mixed into the text being sanitised.

use super::caps::{Capabilities, OscProfile};

/// The escape that opens every sequence here.
const OSC: &str = "\x1b]";
/// The fixed start of every OSC 0 payload.
const TITLE_PREFIX: &str = "ɛ । ";
/// BEL, and not ST (`ESC \`).
///
/// Both are legal and every terminal in the matrix accepts both. BEL is
/// chosen because it is one byte and because a stray ST inside a stream that
/// something else re-frames (a PTY recorder, a log shipper) is more likely to
/// be mangled than a stray BEL.
const END: &str = "\x07";

/// Where a run is, for OSC 9;4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Clear the indicator. Emitted when a run ends, on every path — a
    /// progress bar left set is a taskbar that stays orange until the user
    /// restarts the terminal.
    Cleared,
    /// A run whose end cannot be predicted, which is every generation: the
    /// number of turns is not known in advance.
    Indeterminate,
    /// A run with a budget, where the fraction spent is meaningful.
    At(u8),
    Failed,
}

/// What the host should think ego is doing. TUIC's `state` verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Busy,
    /// Blocked on a person. TUIC turns this into a confident question badge,
    /// which is the signal its own documentation says the heuristic path
    /// cannot produce reliably.
    Awaiting,
}

/// The emitter, bound to one profile.
///
/// A value rather than free functions so a caller cannot emit a sequence the
/// gate did not allow: there is no way to ask for OSC 7770 without holding an
/// `Osc` built from an [`OscProfile::Tuic`] capability.
/// A value rather than free functions so a caller cannot emit a sequence the
/// gate did not allow: there is no way to ask for OSC 7770 without holding an
/// `Osc` built from an [`OscProfile::Tuic`] capability, and no way to ask for a
/// hyperlink on a host [`Capabilities`] found hostile to one.
#[derive(Debug, Clone, Copy)]
pub struct Osc {
    profile: OscProfile,
    /// Both carried rather than re-derived: the two exceptions in the matrix
    /// are decided in `caps.rs` from the environment, and a second copy of
    /// either rule here would be a second chance to get it wrong.
    hyperlinks: bool,
    progress: bool,
}

impl Osc {
    pub fn new(capabilities: Capabilities) -> Self {
        Osc {
            profile: capabilities.osc,
            hyperlinks: capabilities.hyperlinks,
            progress: capabilities.progress,
        }
    }

    fn on(&self) -> bool {
        self.profile != OscProfile::None
    }

    /// OSC 0 — window and icon title, always carrying the fixed brand prefix.
    ///
    /// Only `text` is dynamic and sanitised. An empty `text` still emits the
    /// prefix, so callers cannot accidentally produce an unbranded payload.
    pub fn title(&self, text: &str) -> String {
        if !self.on() {
            return String::new();
        }
        format!("{OSC}0;{TITLE_PREFIX}{}{END}", sanitise(text))
    }

    /// OSC 7 — the working directory, as a `file://` URL.
    ///
    /// TUIC reassigns a pane to a different worktree when this changes
    /// (`tuicommander/docs/frontend/terminal-features.md`), so ego reporting
    /// its workspace root is what makes an `ego -C …` run appear under the
    /// right branch instead of the one the shell was in.
    pub fn cwd(&self, path: &str) -> String {
        if !self.on() {
            return String::new();
        }
        format!("{OSC}7;file://{}{END}", sanitise(path))
    }

    /// OSC 8 — a hyperlink around `text`.
    ///
    /// The empty middle field is the (optional) `id=` parameter list. It is
    /// left empty deliberately: an id makes two links with the same id
    /// highlight together, which is right for a wrapped URL and wrong for two
    /// separate mentions of the same file.
    pub fn hyperlink(&self, uri: &str, text: &str) -> String {
        if !self.hyperlinks {
            return text.to_string();
        }
        format!(
            "{OSC}8;;{}{END}{text}{OSC}8;;{END}",
            sanitise_uri(uri),
            text = text
        )
    }

    /// A `file://` link to a path, optionally at a line.
    ///
    /// The `#line` fragment is what iTerm2 and the editors that register a
    /// `file://` handler read. A terminal that does not understand the
    /// fragment opens the file, which is the correct degradation.
    pub fn file_link(&self, path: &str, line: Option<usize>, text: &str) -> String {
        let uri = match line {
            Some(line) => format!("file://{path}#{line}"),
            None => format!("file://{path}"),
        };
        self.hyperlink(&uri, text)
    }

    /// OSC 9;4 — the taskbar/tab progress indicator.
    pub fn progress(&self, progress: Progress) -> String {
        if !self.progress {
            return String::new();
        }
        let (state, value) = match progress {
            Progress::Cleared => (0, 0),
            Progress::At(percent) => (1, percent.min(100)),
            Progress::Failed => (2, 0),
            Progress::Indeterminate => (3, 0),
        };
        format!("{OSC}9;4;{state};{value}{END}")
    }

    /// A desktop notification, spelled the way this host understands.
    ///
    /// Not both spellings: Ghostty supports 9 *and* 777, so emitting both
    /// notifies twice. The ADR's matrix is what decides, and it decides here
    /// rather than at the call site.
    pub fn notify(&self, title: &str, body: &str) -> String {
        match self.profile {
            OscProfile::None => String::new(),
            OscProfile::Tuic => format!(
                "{OSC}777;notify;{};{}{END}",
                sanitise(title),
                sanitise(body)
            ),
            // OSC 9 carries one string. The title is prefixed rather than
            // dropped, because the body alone ("done in 4m 12s") does not say
            // which of six panes it came from.
            OscProfile::Standard => {
                format!("{OSC}9;{}: {}{END}", sanitise(title), sanitise(body))
            }
        }
    }

    /// Blocked on a person: the awaiting state and, when `notify`, one
    /// notification naming the tool.
    ///
    /// Emitted only where the question is put to the person (the terminal
    /// owner's modal), never from the permission *request* event: a judge or a
    /// stored rule can still settle a request after it is raised, and nobody
    /// should be told to come back for a question that answers itself.
    pub fn awaiting(&self, tool: &str, notify: bool) -> String {
        let notification = if notify {
            self.notify(
                crate::DISPLAY_NAME,
                &format!("permission needed for {tool}"),
            )
        } else {
            String::new()
        };
        [self.state(State::Awaiting), notification].concat()
    }

    /// OSC 133 A — a prompt is about to be drawn.
    pub fn prompt_start(&self) -> String {
        self.osc133("A", None)
    }

    /// OSC 133 C — the command has been submitted and output begins.
    ///
    /// This is the mark that earns its keep: TUIC segments a pane into command
    /// blocks from 133 A/C/D, and falls back to pattern-matching tool-call
    /// headers for agents that emit neither
    /// (`tuicommander/docs/FEATURES.md:187`). Three writes replace a heuristic.
    pub fn output_start(&self) -> String {
        self.osc133("C", None)
    }

    /// OSC 133 D — the command finished, with its exit status.
    pub fn output_end(&self, code: i32) -> String {
        self.osc133("D", Some(&code.to_string()))
    }

    fn osc133(&self, command: &str, argument: Option<&str>) -> String {
        if !self.on() {
            return String::new();
        }
        match argument {
            Some(argument) => format!("{OSC}133;{command};{}{END}", sanitise(argument)),
            None => format!("{OSC}133;{command}{END}"),
        }
    }

    /// OSC 7770 — TUICommander's in-band protocol. Silent everywhere else.
    fn tuic(&self, verb: &str, payload: &str) -> String {
        if self.profile != OscProfile::Tuic {
            return String::new();
        }
        format!("{OSC}7770;{verb}={}{END}", sanitise(payload))
    }

    pub fn state(&self, state: State) -> String {
        self.tuic(
            "state",
            match state {
                State::Idle => "idle",
                State::Busy => "busy",
                State::Awaiting => "awaiting",
            },
        )
    }

    /// What ego is doing, for the pane's context bar.
    pub fn intent(&self, text: &str) -> String {
        self.tuic("intent", text)
    }
}

/// Strip everything that could end this OSC or start another one.
///
/// C0 controls go, `\x7f` goes, and ESC goes. Nothing is escaped or replaced
/// with a marker: a payload is a label, and a label that silently loses a
/// control character is better than one that carries a mechanism.
fn sanitise(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .collect()
}

/// The same, plus the two characters that would be read as OSC 8's own field
/// separators.
///
/// A URI containing `;` splits the sequence into a different parameter list —
/// which for OSC 8 means the *rest of the URL becomes the link text position*.
/// Percent-encoding is the correct answer for a URI and is what this does for
/// the two structural characters; the rest of the string is left alone,
/// because a path with a space in it is a path a terminal opens correctly and
/// an over-eager encoder would break.
fn sanitise_uri(uri: &str) -> String {
    // `%` first, or the `%3B` written by the next step would itself be
    // encoded into `%253B`.
    sanitise(uri).replace('%', "%25").replace(';', "%3B")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that consumes everything in the matrix.
    fn capable(profile: OscProfile) -> Capabilities {
        Capabilities {
            depth: super::super::caps::Depth::TrueColor,
            width: 80,
            hyperlinks: profile != OscProfile::None,
            progress: profile != OscProfile::None,
            osc: profile,
        }
    }

    fn tuic() -> Osc {
        Osc::new(capable(OscProfile::Tuic))
    }

    fn standard() -> Osc {
        Osc::new(capable(OscProfile::Standard))
    }

    fn off() -> Osc {
        Osc::new(Capabilities::plain())
    }

    #[test]
    fn a_pipe_receives_no_sequence_at_all() {
        let osc = off();
        assert_eq!(osc.title("εgo"), "");
        assert_eq!(osc.cwd("/ws"), "");
        assert_eq!(osc.progress(Progress::Indeterminate), "");
        assert_eq!(osc.notify("εgo", "done"), "");
        assert_eq!(osc.prompt_start(), "");
        assert_eq!(osc.output_end(0), "");
        assert_eq!(osc.state(State::Busy), "");
        // A hyperlink degrades to its text rather than to nothing: the link
        // was carrying a word, and dropping the word would delete content.
        assert_eq!(osc.file_link("/ws/a.rs", Some(4), "a.rs:4"), "a.rs:4");
    }

    #[test]
    fn every_title_payload_has_the_fixed_prefix_byte_for_byte() {
        let osc = standard();

        assert_eq!(osc.title("εgo"), "\x1b]0;ɛ । εgo\x07");
        assert_eq!(osc.title(""), "\x1b]0;ɛ । \x07");
        assert_eq!(
            osc.title("safe\x07 title\x1b\n"),
            "\x1b]0;ɛ । safe title\x07"
        );
    }

    /// The private protocol is unreachable without the profile that proves the
    /// host. Not a convention — there is no public method that takes a verb.
    #[test]
    fn only_tuic_gets_tuics_protocol() {
        assert_eq!(standard().state(State::Busy), "");
        assert_eq!(standard().intent("reading files"), "");
        assert_eq!(tuic().state(State::Busy), "\x1b]7770;state=busy\x07");
        assert_eq!(
            tuic().intent("reading files"),
            "\x1b]7770;intent=reading files\x07"
        );
    }

    /// Ghostty supports OSC 9 *and* OSC 777. Emitting both would notify twice,
    /// which is worse than not notifying: a duplicate is a bug the user has to
    /// reason about.
    #[test]
    fn a_notification_is_spelled_once_per_host() {
        assert_eq!(
            tuic().notify("εgo", "turn finished"),
            "\x1b]777;notify;εgo;turn finished\x07"
        );
        assert_eq!(
            standard().notify("εgo", "turn finished"),
            "\x1b]9;εgo: turn finished\x07"
        );
    }

    /// The injection this module exists to prevent. A model that writes a BEL
    /// into its intent line would otherwise close the sequence and put the
    /// remainder on the screen; an ESC would let it start a sequence of its
    /// own choosing.
    #[test]
    fn a_payload_cannot_close_the_sequence_it_is_inside() {
        let hostile = "ok\x07 rm -rf / \x1b[31mred\x1b[0m\nsecond line";
        let emitted = tuic().intent(hostile);
        assert_eq!(
            emitted,
            "\x1b]7770;intent=ok rm -rf / [31mred[0msecond line\x07"
        );
        // Exactly one opener and one terminator, whatever the payload said.
        assert_eq!(emitted.matches('\x1b').count(), 1);
        assert_eq!(emitted.matches('\x07').count(), 1);
    }

    #[test]
    fn a_notification_body_is_sanitised_on_both_fields() {
        let emitted = standard().notify("ti\x07tle", "bo\x1bdy");
        assert_eq!(emitted.matches('\x07').count(), 1);
        assert_eq!(emitted.matches('\x1b').count(), 1);
    }

    /// A `;` inside an OSC 8 URI is not cosmetic: it splits the sequence's
    /// parameter list, so the tail of the URL is read as the *next* field.
    #[test]
    fn a_semicolon_cannot_split_a_hyperlink() {
        let emitted = standard().hyperlink("file:///ws/a;b.rs", "a;b.rs");
        assert!(emitted.contains("file:///ws/a%3Bb.rs"));
        // The visible text is untouched — it is text, not a parameter.
        assert!(emitted.contains("\x07a;b.rs\x1b"));
    }

    #[test]
    fn a_file_link_names_the_line_when_there_is_one() {
        assert_eq!(
            standard().file_link("/ws/src/run.rs", Some(42), "run.rs:42"),
            "\x1b]8;;file:///ws/src/run.rs#42\x07run.rs:42\x1b]8;;\x07"
        );
        assert_eq!(
            standard().file_link("/ws/src/run.rs", None, "run.rs"),
            "\x1b]8;;file:///ws/src/run.rs\x07run.rs\x1b]8;;\x07"
        );
    }

    #[test]
    fn progress_states_are_the_documented_ones() {
        let osc = standard();
        assert_eq!(osc.progress(Progress::Cleared), "\x1b]9;4;0;0\x07");
        assert_eq!(osc.progress(Progress::At(37)), "\x1b]9;4;1;37\x07");
        assert_eq!(osc.progress(Progress::Failed), "\x1b]9;4;2;0\x07");
        assert_eq!(osc.progress(Progress::Indeterminate), "\x1b]9;4;3;0\x07");
    }

    /// The two hosts in the matrix that would *draw* a sequence rather than
    /// discard it. Both refusals are decided in `caps.rs` and carried here, so
    /// the emitter cannot re-derive one of them differently.
    ///
    /// A hyperlink degrades to its text; an indicator degrades to nothing,
    /// because there is no text an indicator was carrying.
    #[test]
    fn a_host_that_would_draw_a_sequence_is_never_sent_it() {
        let hostile = Osc::new(Capabilities {
            hyperlinks: false,
            progress: false,
            ..capable(OscProfile::Standard)
        });
        assert_eq!(hostile.file_link("/ws/a.rs", Some(4), "a.rs:4"), "a.rs:4");
        assert_eq!(hostile.progress(Progress::Indeterminate), "");
        // Everything else still goes: the two exceptions are two sequences,
        // not a downgrade of the whole surface.
        assert_eq!(hostile.title("εgo"), "\x1b]0;ɛ । εgo\x07");
        assert_eq!(hostile.output_start(), "\x1b]133;C\x07");
        assert_eq!(hostile.notify("εgo", "done"), "\x1b]9;εgo: done\x07");
    }

    /// A percentage above 100 is a budget computed from a figure that grew.
    /// Clamped rather than asserted: the ADR's failure semantics say a
    /// rendering defect must not be able to take a run with it.
    #[test]
    fn a_percentage_over_a_hundred_is_clamped_not_panicked() {
        assert_eq!(standard().progress(Progress::At(240)), "\x1b]9;4;1;100\x07");
    }

    #[test]
    fn the_three_shell_integration_marks_are_what_tuic_parses() {
        let osc = standard();
        assert_eq!(osc.prompt_start(), "\x1b]133;A\x07");
        assert_eq!(osc.output_start(), "\x1b]133;C\x07");
        assert_eq!(osc.output_end(0), "\x1b]133;D;0\x07");
        assert_eq!(osc.output_end(3), "\x1b]133;D;3\x07");
    }

    #[test]
    fn a_wait_is_the_state_plus_one_notification_spelled_for_the_host() {
        assert_eq!(
            tuic().awaiting("bash", true),
            "\x1b]7770;state=awaiting\x07\x1b]777;notify;εgo;permission needed for bash\x07"
        );
        assert_eq!(
            standard().awaiting("bash", true),
            "\x1b]9;εgo: permission needed for bash\x07"
        );
        // Quiet drops the notification and never the state.
        assert_eq!(
            tuic().awaiting("bash", false),
            "\x1b]7770;state=awaiting\x07"
        );
        assert_eq!(off().awaiting("bash", true), "");
    }
}
