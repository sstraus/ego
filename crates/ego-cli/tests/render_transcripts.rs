//! Golden transcripts: one markdown document, six renderings.
//!
//! # What these goldens are, and what they are worth
//!
//! The unit tests under `src/render/` each assert one rule — that a strong span
//! carries weight and no colour, that a table too narrow to draw becomes a
//! list. Every one of them is true of a renderer that is wrong about
//! *everything else*, because none of them looks at a whole document.
//!
//! These do. Each file is what `ego` writes for one width at one depth, and it
//! is committed, so a change to any rule shows up as a diff in a page of text a
//! person can read. That is the point: a renderer's defects are almost never
//! visible in a unit assertion — they are a heading that lost its rule, a list
//! continuation that lost its column, a table that grew a cell past the right
//! edge. All three are obvious in a transcript and invisible in a `contains`.
//!
//! These goldens prove renderer output bytes, not what a particular terminal
//! draws. They do not establish emulator capability support.
//!
//! **Regenerating them is the last resort, not the first.** A diff here means
//! the rendering changed. Read the diff, decide whether the new rendering is
//! better, and only then write the files — `cargo test -p ego-cli --test
//! render_transcripts -- --ignored` does it, and its output is what to review.
//!
//! The three widths are not arbitrary. 40 is a split pane, where every cell of
//! the table has to wrap and the layout is under real pressure; 80 is the width
//! a redirected run uses; 120 is a window somebody actually works in, and the
//! one that shows whether anything stretches to fill space it does not need.

use ego_cli::render::caps::Depth;
use ego_cli::render::{block, markdown};

const SOURCE: &str = include_str!("goldens/render/source.md");

const WIDTHS: [usize; 3] = [40, 80, 120];

/// The rendering of `SOURCE` at one width and one depth.
fn transcript(width: usize, depth: Depth) -> String {
    block(&markdown::render(SOURCE, width), depth)
}

/// The two depths that are contracts rather than fallbacks: what a modern
/// terminal gets, and what a file gets. The 256 and 16 rungs are quantisations
/// of the first and are asserted where the quantisation lives, in `style.rs`.
fn goldens() -> Vec<(String, Depth, String)> {
    let mut out = Vec::new();
    for width in WIDTHS {
        out.push((format!("{width}.ansi"), Depth::TrueColor, String::new()));
        out.push((format!("{width}.txt"), Depth::None, String::new()));
    }
    for (index, entry) in out.iter_mut().enumerate() {
        entry.2 = transcript(WIDTHS[index / 2], entry.1);
    }
    out
}

fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/goldens/render")
        .join(name)
}

#[test]
fn the_document_renders_the_way_it_is_committed() {
    for (name, _, produced) in goldens() {
        let path = golden_path(&name);
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        assert_eq!(
            produced, committed,
            "{name} changed. Read the diff before regenerating it: \
             cargo test -p ego-cli --test render_transcripts -- --ignored"
        );
    }
}

/// Not a test — the way the files above are written. `--ignored` so it never
/// runs in a gate, because a golden that regenerates itself asserts nothing.
#[test]
#[ignore = "writes the goldens; run deliberately after reading the diff"]
fn regenerate() {
    for (name, _, produced) in goldens() {
        let path = golden_path(&name);
        std::fs::write(&path, &produced).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        println!("wrote {}", path.display());
    }
}

/// Catches: paragraphs acquire renderer hard breaks, or structured rows overflow.
/// Prose stays on logical lines and the terminal owns its soft wrapping. Lists,
/// quotes and tables keep width-constrained layout; verbatim code and oversized
/// words retain their existing overflow exceptions.
#[test]
fn paragraphs_keep_logical_lines_and_structured_rows_keep_their_width() {
    let paragraphs = [
        "A paragraph with strong weight, emphasis, a retraction and inline_code() in it, long enough that it has to wrap at every width this document is rendered at.",
        "Syntax highlighting writes a foreground; a diff writes a background. They compose on one cell grid. See the decision record.",
        "Loose lists are the case a tight one hides:",
        "The last paragraph.",
    ];
    let verbatim = code_lines();
    for width in WIDTHS {
        let rendered = transcript(width, Depth::None);
        for paragraph in paragraphs {
            assert_eq!(
                rendered.lines().filter(|line| *line == paragraph).count(),
                1,
                "at {width} columns the logical paragraph changed: {paragraph:?}"
            );
            let mut terminal = vt100::Parser::new(12, width as u16, 0);
            terminal.process(format!("{paragraph}\r\n").as_bytes());
            assert_eq!(
                terminal.screen().row_wrapped(0),
                ego_cli::render::line::cells(paragraph) > width,
                "the terminal must own paragraph wrapping at {width} columns"
            );
        }
        for line in rendered.lines() {
            if paragraphs.contains(&line) {
                continue;
            }
            if verbatim.iter().any(|code| line.trim_end().ends_with(code)) {
                continue;
            }
            if line
                .split_whitespace()
                .any(|word| ego_cli::render::line::cells(word) > width)
            {
                continue;
            }
            assert!(
                ego_cli::render::line::cells(line) <= width,
                "at {width} columns: {line:?}"
            );
        }
    }
}

/// The contents of every fenced block in the fixture, read from the fixture
/// rather than listed here — a hard-coded copy would go stale the moment the
/// document changes and would then silently excuse a real overflow.
fn code_lines() -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in SOURCE.lines() {
        if line.starts_with("```") {
            inside = !inside;
        } else if inside && !line.trim().is_empty() {
            out.push(line.to_string());
        }
    }
    out
}

/// Piped output is a distinct plain rendering, not escaped terminal output
/// with color stripped. It contains neither escapes nor trailing padding.
#[test]
fn a_file_gets_no_escapes_and_no_ragged_blanks() {
    for width in WIDTHS {
        let plain = transcript(width, Depth::None);
        assert!(!plain.contains('\x1b'), "at {width} columns");
        for line in plain.lines() {
            assert_eq!(line.trim_end(), line, "at {width} columns: {line:?}");
        }
    }
}

/// Colour costs no content. Whatever the depth, the same words are on the
/// screen — which is the property that lets `--no-color` be a rendering
/// decision rather than a feature switch.
#[test]
fn the_two_depths_differ_in_escapes_and_never_in_words() {
    for width in WIDTHS {
        let coloured = transcript(width, Depth::TrueColor);
        let plain = transcript(width, Depth::None);
        assert!(coloured.contains('\x1b'), "at {width} columns");

        let stripped: String = strip(&coloured)
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            stripped.trim_end(),
            plain.trim_end(),
            "at {width} columns the colours changed the text"
        );
    }
}

/// A three-column table is a grid at every width here, including 40 where its
/// cells have to wrap to manage it. Every row of one table is the same width —
/// the defect a table renderer produces is a row that disagrees with the rule
/// above it — and none of them is wider than the terminal.
///
/// Not *equal* to the terminal: a table takes the width its content needs and
/// stops. Stretching a three-column table across 120 cells to fill them would
/// be padding presented as layout.
#[test]
fn every_row_of_the_table_agrees_and_none_reaches_past_the_edge() {
    for width in WIDTHS {
        let drawn = transcript(width, Depth::None);
        let grid: Vec<usize> = drawn
            .lines()
            .filter(|line| line.starts_with(['┌', '│', '├', '└']))
            .map(ego_cli::render::line::cells)
            .collect();
        assert!(!grid.is_empty(), "at {width} columns");
        assert!(
            grid.iter().all(|drawn| *drawn == grid[0]),
            "at {width} columns the rows disagree: {grid:?}"
        );
        assert!(grid[0] <= width, "at {width} columns: {}", grid[0]);
    }
    // And the narrowest of them does use the whole terminal, because at 40 its
    // content needs every cell.
    let narrow = transcript(40, Depth::None);
    assert!(
        narrow
            .lines()
            .any(|line| line.starts_with('┌') && ego_cli::render::line::cells(line) == 40)
    );
}

/// The case the widths above do not reach: a terminal too narrow for a grid at
/// all. Not a golden, because there is no rendering to review — the claim is
/// that the *values* survive, which is what truncating a cell would break.
#[test]
fn a_table_too_narrow_to_draw_still_says_everything_it_said() {
    let narrow = block(&markdown::render(SOURCE, 20), Depth::None);
    assert!(
        !narrow.contains('┌'),
        "20 columns cannot hold three columns"
    );
    for value in [
        "OSC 8",
        "hyperlink",
        "OSC 133",
        "command blocks, so a host stops guessing",
        "OSC 7770",
        "state and intent",
        "yes, and nowhere else",
    ] {
        // Wrapped, so the words survive rather than the string: the claim is
        // that nothing was dropped, not that nothing was re-flowed.
        for word in value.split_whitespace() {
            assert!(narrow.contains(word), "{value:?} lost {word:?}");
        }
    }
}

/// SGR removed, so a test can compare what is displayed.
fn strip(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        match rest[at..].find(|character: char| character.is_ascii_alphabetic()) {
            Some(end) => rest = &rest[at + end + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}
