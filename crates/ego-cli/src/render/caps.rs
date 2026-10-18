//! What this terminal can be told, decided once.
//!
//! `ARCHITECTURE.md` §1: no
//! call site reads an environment variable and no call site formats an escape
//! by hand. Everything a renderer is allowed to do is a field on
//! [`Capabilities`], and [`Capabilities::of`] is the only function that decides
//! one.
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
///
/// Not a bitfield of individual sequences: the matrix in the ADR has three
/// columns because there are three hosts, and the two places they actually
/// differ — how a notification is spelled, and whether 7770 means anything —
/// are decided together or not at all. A bitfield would let a caller ask for
/// TUIC's private protocol on iTerm2.
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
    /// Whether stdout — not stderr, not stdin — is a terminal. Stdout is what
    /// carries the answer, and `CLI.md` §4.5 makes it the channel that has to
    /// stay clean when it is not one.
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
///
/// A terminal that reports 3 columns, or 0, is not a terminal anybody is
/// reading; layout that divides by the reported width would either panic or
/// produce one character per line. Clamping is total where asserting is not,
/// which the ADR's failure semantics require of layout.
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
    ///
    /// A screen is necessary and not sufficient. §4 of the ADR rests on "an
    /// unknown OSC is discarded", and that is true of every host in the matrix
    /// — but **not** of Apple's Terminal.app, which garbles unknown OSC rather
    /// than swallowing it. That is not a guess: it is the reason xAI's own
    /// harness classifies it as a hostile parser and sends it no hyperlink
    /// (`crates/codegen/xai-grok-pager-render/src/terminal/hyperlinks.rs:8`).
    ///
    /// The cost of being wrong is asymmetric, which is what decides it. A
    /// hyperlink not emitted costs a click; a hyperlink emitted to a parser
    /// that mangles it costs the line it was on.
    fn hyperlinks(terminal: &Terminal<'_>, osc: OscProfile) -> bool {
        osc != OscProfile::None && terminal.term_program != Some("Apple_Terminal")
    }

    /// Whether OSC 9;4 is safe here.
    ///
    /// iTerm2 gained the progress indicator in 3.6. **Before** 3.6 it read the
    /// same bytes as an OSC 9 *desktop notification* and displayed the raw
    /// parameters — `4;1;-1` — as the alert text. So on an older iTerm2 the
    /// sequence is not ignored, it is rendered, which is exactly what §4 says
    /// ego does not do. An unparseable or absent version is treated as old:
    /// the fallback that loses an indicator is preferable to the one that pops
    /// an alert saying `4;3;0`.
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

    /// What a run that will never be looked at gets: nothing.
    ///
    /// Used by `--output json`/`jsonl`/`acp`, where §4.5's "stdout is
    /// protocol-clean" is not a preference about appearance. A JSON document
    /// with an SGR inside it is not JSON.
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

    /// Whether anything is being drawn, as opposed to written down.
    ///
    /// The distinction §2 rests on. It is deliberately *not* `coloured()`:
    /// `--no-color` on a real terminal is a person asking for no colour, not a
    /// person asking for a file — the layout, the tables and the wrap column
    /// still belong to the window they are looking at. Only a run with no
    /// screen at all reverts to reproducing its input.
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
        // Every terminal in the ADR's matrix does truecolour and says so
        // through `TERM_PROGRAM` when it does not export `COLORTERM` —
        // including TUIC, which reports `ghostty`.
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

    /// Apple's Terminal.app does not discard an OSC it does not understand —
    /// it garbles it, and the garbage lands on the line the link was on. It is
    /// the one host in the matrix for which §4's "an unknown OSC is discarded"
    /// is false, so it is the one host that gets no hyperlink.
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

    /// Before 3.6, iTerm2 read OSC 9;4 as an OSC 9 *notification* and popped an
    /// alert reading `4;3;0`. So the version is not a nicety here — below it
    /// the sequence is drawn rather than ignored, which is the one thing §4
    /// promises ego never does.
    ///
    /// Unknown means old. Losing an indicator is a smaller failure than
    /// showing somebody an alert box containing three semicolons.
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

    /// The defect this whole module exists to fix: `--no-color` was parsed and
    /// never read (`cli.rs:382` → `cli.rs:951` → nothing), so `CLI.md` §3.1
    /// marked **yes** a flag that changed no byte.
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

    /// §2 of the ADR: a redirected run must produce the same bytes whatever
    /// terminal launched it. Taking the wrap column from the window would make
    /// `ego run … > out.md` depend on how wide the window happened to be.
    #[test]
    fn a_pipe_wraps_at_a_fixed_column_not_at_the_windows() {
        let piped = Terminal {
            stdout_is_terminal: false,
            columns: Some(203),
            ..rich()
        };
        assert_eq!(Capabilities::of(&piped).width, PIPE_WIDTH);
    }

    /// Layout divides by the width. A terminal reporting a width no layout can
    /// use gets a floor, because the ADR's failure semantics say layout is
    /// total over its inputs rather than asserted.
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

    /// §4.5 is not a preference about appearance: an SGR inside a JSON
    /// document makes it stop being one.
    #[test]
    fn the_machine_readable_modes_have_nothing_to_negotiate() {
        let plain = Capabilities::plain();
        assert_eq!(plain.depth, Depth::None);
        assert_eq!(plain.osc, OscProfile::None);
        assert!(!plain.hyperlinks);
    }
}
