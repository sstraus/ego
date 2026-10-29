//! What this terminal can be told, decided once.
//!
//! Renderers read capability fields, not environment variables or hand-built
//! escapes. [`Capabilities::of`] is the sole capability decision point.
//!
//! The split into [`Terminal`] (what was observed) and [`Capabilities`] (what
//! follows from it) is what makes the decision testable at all: the inputs are
//! process-global — `std::env::var`, `isatty`, an ioctl — and a rule expressed
//! over globals can only be checked by a test that changes them for every other
//! test in the binary. Here the rule is a pure function of a struct.

use std::io::IsTerminal;

/// How many colours may be named.
///
/// Ordered from nothing to everything so a comparison reads as "at least":
/// `depth >= Depth::Ansi256`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Depth {
    /// No SGR at all. Not "black and white" — *no escapes*, so the output is
    /// the same bytes a file would get.
    None,
    /// The eight basics and their bright forms (SGR 30-37, 90-97).
    Ansi16,
    /// The xterm cube and greyscale ramp (SGR 38;5;n).
    Ansi256,
    /// SGR 38;2;r;g;b. Every terminal in the matrix supports it; the lower two
    /// rungs exist for the ones that are not in the matrix.
    TrueColor,
}

/// Which family of OSC sequences this host understands.
/// Notification spelling and private OSC support are selected together by
/// host profile. Independent bits could select TUIC sequences on iTerm2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscProfile {
    /// A pipe, a file, `TERM=dumb`. Nothing is emitted.
    None,
    /// Anything that draws a terminal: title, cwd, hyperlink, progress,
    /// shell-integration marks, and OSC 9 for a notification.
    Standard,
    /// TUICommander, proven by `TUIC_SOCKET` in the environment rather than
    /// guessed from `TERM_PROGRAM` — which TUIC deliberately sets to
    /// `ghostty` to satisfy other agents' allow-lists
    /// (`tuicommander/docs/backend/pty.md:351`), so the obvious signal is the
    /// one signal that is known to lie.
    ///
    /// Adds OSC 7770 and moves notification from 9 to 777, which is what TUIC
    /// parses (`docs/backend/output-parser.md`).
    Tuic,
}

/// What was observed about the process's standard output and environment.
///
/// Borrowed rather than owned so `from_process` can hand over what it read
/// without a copy, and so a test can write a case in one expression.
#[derive(Debug, Default, Clone)]
pub struct Terminal<'a> {
    /// `--no-color` on the command line. Highest precedence: a person who
    /// typed it is not asking for a negotiation.
    pub flag_no_color: bool,
    /// Explicit append-only, unstyled output.
    pub flag_plain: bool,
    /// Whether stdout is a terminal; stderr and stdin do not establish output
    /// capabilities.
    pub stdout_is_terminal: bool,
    /// Columns, when anything could report them.
    pub columns: Option<usize>,
    /// `NO_COLOR`. Present and non-empty disables colour, per the informal
    /// standard; the standard is explicit that the *value* carries no meaning,
    /// so `NO_COLOR=0` still disables.
    pub no_color: Option<&'a str>,
    pub term: Option<&'a str>,
    pub colorterm: Option<&'a str>,
    pub term_program: Option<&'a str>,
    /// `TERM_PROGRAM_VERSION`. Read for exactly one decision — see
    /// [`Capabilities::progress`] — because one host in the matrix changed what
    /// a sequence *means* between releases, which is the only thing a version
    /// number is allowed to settle here.
    pub term_program_version: Option<&'a str>,
    /// TUICommander's per-pane socket. Its presence is the only evidence this
    /// build accepts for OSC 7770.
    pub tuic_socket: Option<&'a str>,
}

impl Terminal<'_> {
    /// Read the process. The one place globals are touched.
    pub fn from_process(flag_no_color: bool, env: &Environment) -> Terminal<'_> {
        Terminal {
            flag_no_color,
            flag_plain: false,
            stdout_is_terminal: std::io::stdout().is_terminal(),
            columns: terminal_size::terminal_size().map(|(w, _)| usize::from(w.0)),
            no_color: env.no_color.as_deref(),
            term: env.term.as_deref(),
            colorterm: env.colorterm.as_deref(),
            term_program: env.term_program.as_deref(),
            term_program_version: env.term_program_version.as_deref(),
            tuic_socket: env.tuic_socket.as_deref(),
        }
    }
}

/// The variables read from the environment, held so [`Terminal`] can borrow
/// them.
///
/// A struct rather than a call per variable at the use site because
/// `std::env::var` returns an owned `String` and `Terminal` borrows: something
/// has to live long enough, and making that explicit is cheaper than making
/// `Terminal` own its strings for the sake of one constructor.
#[derive(Debug, Default)]
pub struct Environment {
    no_color: Option<String>,
    term: Option<String>,
    colorterm: Option<String>,
    term_program: Option<String>,
    term_program_version: Option<String>,
    tuic_socket: Option<String>,
}

impl Environment {
    pub fn read() -> Self {
        let var = |key: &str| std::env::var(key).ok();
        Environment {
            no_color: var("NO_COLOR"),
            term: var("TERM"),
            colorterm: var("COLORTERM"),
            term_program: var("TERM_PROGRAM"),
            term_program_version: var("TERM_PROGRAM_VERSION"),
            tuic_socket: var("TUIC_SOCKET"),
        }
    }
}

/// What follows from a [`Terminal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub depth: Depth,
    /// Usable columns, never zero and never absurd. See [`Capabilities::of`].
    pub width: usize,
    /// OSC 8. Needs a screen *and* a host that discards what it does not
    /// understand — see [`Capabilities::hyperlinks`].
    pub hyperlinks: bool,
    /// OSC 9;4, the taskbar indicator. Separate from [`Self::osc`] for the same
    /// reason [`Self::hyperlinks`] is: one host in the matrix draws it as text.
    pub progress: bool,
    pub osc: OscProfile,
}

/// The floor for `width`.
/// Clamp unusable widths instead of asserting. Zero or tiny dimensions
/// must not panic or turn layout into one character per line.
const MIN_WIDTH: usize = 20;

/// What a pipe gets. Not the terminal's width — a redirected run must produce
/// the same bytes from any terminal, so the wrap column cannot come from one.
const PIPE_WIDTH: usize = 80;

impl Capabilities {
    /// The whole rule, in evaluation order.
    pub fn of(terminal: &Terminal<'_>) -> Self {
        if terminal.flag_plain {
            return Self::plain();
        }
        let osc = Self::osc_profile(terminal);
        let depth = Self::depth(terminal);
        let width = match terminal.columns {
            Some(columns) if terminal.stdout_is_terminal => columns.max(MIN_WIDTH),
            _ => PIPE_WIDTH,
        };
        Capabilities {
            depth,
            width,
            hyperlinks: Self::hyperlinks(terminal, osc),
            progress: Self::progress(terminal, osc),
            osc,
        }
    }

    /// Refresh layout geometry without renegotiating the output protocol.
    /// A long-lived interactive sink must not retain the startup pane width.
    pub fn with_current_width(mut self) -> Self {
        if self.on_screen()
            && let Some((width, _)) = terminal_size::terminal_size()
        {
            self.width = usize::from(width.0).max(1);
        }
        self
    }

    /// Whether OSC 8 is safe here.
    /// A screen alone does not establish OSC support. Unsupported sequences
    /// can corrupt visible text, whereas omitting a hyperlink only loses a click.
    /// Use the capability profile rather than emitting one optimistically.
    fn hyperlinks(terminal: &Terminal<'_>, osc: OscProfile) -> bool {
        osc != OscProfile::None && terminal.term_program != Some("Apple_Terminal")
    }

    /// Enable OSC 9;4 only on supported iTerm2 versions. Older releases interpret it as a
    /// desktop notification; an unknown version loses the indicator rather than displaying raw
    /// control parameters.
    fn progress(terminal: &Terminal<'_>, osc: OscProfile) -> bool {
        if osc == OscProfile::None {
            return false;
        }
        if terminal.term_program != Some("iTerm.app") {
            return true;
        }
        terminal
            .term_program_version
            .and_then(|version| {
                let mut parts = version.split('.');
                let major: u32 = parts.next()?.parse().ok()?;
                let minor: u32 = parts.next().unwrap_or("0").parse().ok()?;
                Some((major, minor) >= (3, 6))
            })
            .unwrap_or(false)
    }

    /// Structured output receives no terminal rendering capabilities. An SGR escape inside JSON
    /// would corrupt its protocol.
    pub fn plain() -> Self {
        Capabilities {
            depth: Depth::None,
            width: PIPE_WIDTH,
            hyperlinks: false,
            progress: false,
            osc: OscProfile::None,
        }
    }

    pub fn coloured(&self) -> bool {
        self.depth != Depth::None
    }

    /// Screen layout is independent of color. `--no-color` on a terminal still uses its width
    /// and layout; only output without a screen reproduces plain input.
    pub fn on_screen(&self) -> bool {
        self.osc != OscProfile::None
    }

    fn osc_profile(terminal: &Terminal<'_>) -> OscProfile {
        if !terminal.stdout_is_terminal || terminal.term.is_none_or(|term| term == "dumb") {
            return OscProfile::None;
        }
        if terminal
            .tuic_socket
            .is_some_and(|socket| !socket.is_empty())
        {
            return OscProfile::Tuic;
        }
        OscProfile::Standard
    }

    fn depth(terminal: &Terminal<'_>) -> Depth {
        // The two overrides, in the order a person would expect them to win:
        // what they typed for this run, then what they set for every run.
        if terminal.flag_no_color {
            return Depth::None;
        }
        if terminal.no_color.is_some_and(|value| !value.is_empty()) {
            return Depth::None;
        }
        if !terminal.stdout_is_terminal {
            return Depth::None;
        }
        let Some(term) = terminal.term else {
            return Depth::None;
        };
        if term == "dumb" {
            return Depth::None;
        }
        // `COLORTERM` is the only positive statement about depth anybody
        // actually sets, and both spellings are in the wild.
        if terminal
            .colorterm
            .is_some_and(|value| value == "truecolor" || value == "24bit")
        {
            return Depth::TrueColor;
        }
        // Known truecolor hosts can report `TERM_PROGRAM` instead of `COLORTERM`,
        // including TUIC's `ghostty` value.
        if matches!(
            terminal.term_program,
            Some("iTerm.app" | "ghostty" | "WezTerm" | "vscode" | "Apple_Terminal")
        ) {
            // Apple's Terminal.app is the one member of that list that is not
            // truecolour: it renders 24-bit SGR by snapping to 256 anyway, so
            // naming 256 here is describing what will be displayed rather than
            // giving up something that would have worked.
            return if terminal.term_program == Some("Apple_Terminal") {
                Depth::Ansi256
            } else {
                Depth::TrueColor
            };
        }
        if term.contains("256color") || term.contains("direct") {
            return Depth::Ansi256;
        }
        Depth::Ansi16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal in the best case, which each test then spoils in one way.
    fn rich<'a>() -> Terminal<'a> {
        Terminal {
            flag_no_color: false,
            flag_plain: false,
            stdout_is_terminal: true,
            columns: Some(120),
            no_color: None,
            term: Some("xterm-256color"),
            colorterm: Some("truecolor"),
            term_program: Some("ghostty"),
            term_program_version: None,
            tuic_socket: None,
        }
    }

    /// Terminal.app can render unknown OSC bytes as garbage, so it receives no hyperlink
    /// sequence.
    #[test]
    fn the_one_parser_that_mangles_what_it_cannot_read_is_sent_no_hyperlink() {
        let apple = Terminal {
            term_program: Some("Apple_Terminal"),
            colorterm: None,
            ..rich()
        };
        let capabilities = Capabilities::of(&apple);
        assert!(!capabilities.hyperlinks);
        // And the rest of the surface is unaffected: this is one sequence
        // withheld, not a host demoted.
        assert_eq!(capabilities.osc, OscProfile::Standard);
        assert!(capabilities.progress);
        assert_eq!(capabilities.depth, Depth::Ansi256);

        assert!(Capabilities::of(&rich()).hyperlinks);
    }

    /// Older iTerm2 interprets OSC 9;4 as a notification. Treat an unknown version as
    /// unsupported rather than risking an alert with raw parameters.
    #[test]
    fn iterm_gets_a_progress_indicator_only_where_it_means_progress() {
        let iterm = |version: Option<&'static str>| Terminal {
            term_program: Some("iTerm.app"),
            term_program_version: version,
            ..rich()
        };
        for old in [
            Some("3.5.14"),
            Some("2.9"),
            Some("3"),
            Some("nightly"),
            None,
        ] {
            assert!(
                !Capabilities::of(&iterm(old)).progress,
                "iTerm2 {old:?} draws the parameters as alert text"
            );
        }
        for new in [Some("3.6.0"), Some("3.6"), Some("4.0.1"), Some("3.10")] {
            assert!(Capabilities::of(&iterm(new)).progress, "iTerm2 {new:?}");
        }
        // Nobody else is asked for a version, because nobody else changed what
        // the sequence means.
        assert!(Capabilities::of(&rich()).progress);
    }

    /// `--no-color` must change emitted bytes, not merely parse successfully.
    #[test]
    fn the_flag_that_did_nothing_now_decides() {
        assert_eq!(Capabilities::of(&rich()).depth, Depth::TrueColor);

        let asked = Terminal {
            flag_no_color: true,
            ..rich()
        };
        assert_eq!(Capabilities::of(&asked).depth, Depth::None);
    }

    /// The informal standard is explicit that the *value* of `NO_COLOR` means
    /// nothing — presence is the whole signal. A build that read it as a
    /// boolean would keep colour on for `NO_COLOR=0`, which is the spelling
    /// somebody reaches for when they want it off.
    #[test]
    fn no_color_is_presence_not_truth() {
        for value in ["1", "0", "false", "no"] {
            let terminal = Terminal {
                no_color: Some(value),
                ..rich()
            };
            assert_eq!(
                Capabilities::of(&terminal).depth,
                Depth::None,
                "NO_COLOR={value} is still NO_COLOR"
            );
        }
        // Empty is the documented escape hatch: set-but-empty is unset.
        let terminal = Terminal {
            no_color: Some(""),
            ..rich()
        };
        assert_eq!(Capabilities::of(&terminal).depth, Depth::TrueColor);
    }

    #[test]
    fn a_pipe_is_never_coloured_however_rich_the_environment_claims_to_be() {
        let piped = Terminal {
            stdout_is_terminal: false,
            ..rich()
        };
        let capabilities = Capabilities::of(&piped);
        assert_eq!(capabilities.depth, Depth::None);
        assert_eq!(capabilities.osc, OscProfile::None);
        assert!(!capabilities.hyperlinks);
    }

    /// Redirected output must not depend on the launching terminal's width.
    #[test]
    fn a_pipe_wraps_at_a_fixed_column_not_at_the_windows() {
        let piped = Terminal {
            stdout_is_terminal: false,
            columns: Some(203),
            ..rich()
        };
        assert_eq!(Capabilities::of(&piped).width, PIPE_WIDTH);
    }

    /// Layout divides by width, so unusable reported widths receive a floor
    /// instead of causing a panic.
    #[test]
    fn an_absurd_width_is_clamped_rather_than_believed() {
        for columns in [0, 1, 7] {
            let narrow = Terminal {
                columns: Some(columns),
                ..rich()
            };
            assert_eq!(Capabilities::of(&narrow).width, MIN_WIDTH);
        }
    }

    // Catches: --plain keeps live cursor updates or colour on a real TTY.
    #[test]
    fn plain_overrides_a_rich_terminal() {
        assert_eq!(
            Capabilities::of(&Terminal {
                flag_plain: true,
                ..rich()
            }),
            Capabilities::plain()
        );
    }

    #[test]
    fn dumb_means_dumb_and_missing_means_dumb() {
        for term in [Some("dumb"), None] {
            let terminal = Terminal { term, ..rich() };
            let capabilities = Capabilities::of(&terminal);
            assert_eq!(capabilities.depth, Depth::None);
            assert_eq!(capabilities.osc, OscProfile::None);
        }
    }

    #[test]
    fn depth_falls_back_through_the_rungs() {
        let no_colorterm = Terminal {
            colorterm: None,
            term_program: None,
            ..rich()
        };
        assert_eq!(Capabilities::of(&no_colorterm).depth, Depth::Ansi256);

        let plain_term = Terminal {
            term: Some("xterm"),
            ..no_colorterm.clone()
        };
        assert_eq!(Capabilities::of(&plain_term).depth, Depth::Ansi16);
    }

    /// `TERM_PROGRAM` cannot be trusted for this: TUIC sets it to `ghostty` on
    /// purpose, to satisfy Claude Code's kitty-protocol allow-list
    /// (`tuicommander/docs/backend/pty.md:351`). The socket is set by TUIC for
    /// its own children and by nothing else.
    #[test]
    fn tuic_is_proven_by_its_socket_and_not_by_the_name_it_wears() {
        let impersonating = Terminal {
            term_program: Some("ghostty"),
            tuic_socket: None,
            ..rich()
        };
        assert_eq!(Capabilities::of(&impersonating).osc, OscProfile::Standard);

        let real = Terminal {
            tuic_socket: Some("/tmp/tuic-proxy.9hHkN5/mcp.sock"),
            ..rich()
        };
        assert_eq!(Capabilities::of(&real).osc, OscProfile::Tuic);

        // Exported-but-empty is how a variable survives a shell that unsets it
        // badly. It is not a socket.
        let hollow = Terminal {
            tuic_socket: Some(""),
            ..rich()
        };
        assert_eq!(Capabilities::of(&hollow).osc, OscProfile::Standard);
    }

    /// Structured JSON must contain no SGR escape sequences.
    #[test]
    fn the_machine_readable_modes_have_nothing_to_negotiate() {
        let plain = Capabilities::plain();
        assert_eq!(plain.depth, Depth::None);
        assert_eq!(plain.osc, OscProfile::None);
        assert!(!plain.hyperlinks);
    }
}
