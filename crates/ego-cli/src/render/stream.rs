//! Rendering an answer that has not finished arriving.
//!
//! A model streams tokens; markdown is a block language. The first two cells
//! of a table say nothing about how wide its columns will be, and a fence is
//! not code until it closes. So a renderer has exactly three choices, and two
//! of them are wrong:
//!
//! Render complete Markdown blocks while retaining the unfinished suffix. Rendering each
//! delta misclassifies partial syntax; buffering an entire message hides progress. A
//! blank line outside a fence establishes a safe block boundary.
//!
//! Redirected output is a different rendering: preserve the model's markdown
//! byte for byte so `ego run … > answer.md` produces a Markdown file, not
//! a terminal redraw transcript.

use pulldown_cmark::{Event, Parser};

use super::caps::Capabilities;
use super::{block, markdown};

/// Buffers a streaming answer and hands back whatever is finished.
pub struct Answer {
    capabilities: Capabilities,
    /// Text received and not yet rendered.
    pending: String,
    /// Whether anything has been emitted, so a run can know it has to close
    /// the last line.
    started: bool,
}

impl Answer {
    pub fn new(capabilities: Capabilities) -> Self {
        Answer {
            capabilities,
            pending: String::new(),
            started: false,
        }
    }

    /// Update the geometry before rendering the next buffered block.
    pub fn resize(&mut self, width: usize) {
        self.capabilities.width = width;
    }

    /// Whether this answer is being drawn rather than piped.
    fn rendering(&self) -> bool {
        self.capabilities.on_screen()
    }

    pub fn started(&self) -> bool {
        self.started
    }

    /// Take a delta; return the bytes that are ready to be written.
    pub fn push(&mut self, delta: &str) -> String {
        if !self.rendering() {
            self.started = true;
            return delta.to_string();
        }
        self.pending.push_str(delta);
        match self.split() {
            Some(at) => {
                let finished: String = self.pending.drain(..at).collect();
                self.render(&finished)
            }
            None => String::new(),
        }
    }

    /// End of message: render whatever is left.
    pub fn finish(&mut self) -> String {
        if !self.rendering() {
            return String::new();
        }
        let rest = std::mem::take(&mut self.pending);
        self.render(&rest)
    }

    fn render(&mut self, source: &str) -> String {
        if source.trim().is_empty() {
            return String::new();
        }
        let lines = markdown::render(source, self.capabilities.width);
        if lines.is_empty() {
            return String::new();
        }
        // A blank line between blocks, but never one before the first: the
        // separation belongs *between* what was flushed and what comes next,
        // and only the writer knows it has written before.
        let separator = if self.started { "\n" } else { "" };
        self.started = true;
        format!("{separator}{}", block(&lines, self.capabilities.depth))
    }

    /// Return the last completed top-level block boundary. Parser depth prevents blank lines
    /// inside lists or blockquotes from splitting one construct. Require source evidence of
    /// completion because parsers close incomplete constructs at end of input. Shared markdown
    /// options keep boundary detection and rendering aligned.
    fn split(&self) -> Option<usize> {
        let mut depth = 0usize;
        let mut last_safe = None;
        for (event, range) in Parser::new_ext(&self.pending, markdown::options()).into_offset_iter()
        {
            match event {
                Event::Start(_) => depth += 1,
                Event::End(_) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 && self.settled(range.end) {
                        last_safe = Some(range.end);
                    }
                }
                _ => {}
            }
        }
        last_safe
    }

    /// Whether the source after `offset` proves the block before it is closed.
    ///
    /// **A blank line, and nothing weaker.** "Some text follows it" is not
    /// proof and was the first thing tried: a table whose delimiter row had
    /// arrived but whose first data row had not parses as a complete
    /// header-only table with a `|` after it, and flushing there printed the
    /// header and threw every row away. The same shape catches a lazy
    /// paragraph continuation, a setext underline and the next item of a list
    /// — in each case the parser will fold the following text back into the
    /// block once enough of it arrives.
    ///
    /// A blank line cannot be folded back into anything. It is the one
    /// separator CommonMark treats as terminal, which is exactly why it is the
    /// one this asks for.
    fn settled(&self, offset: usize) -> bool {
        self.pending[offset..]
            .trim_start_matches([' ', '\t'])
            .starts_with('\n')
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::caps::{Depth, OscProfile};

    fn screen() -> Capabilities {
        Capabilities {
            depth: Depth::TrueColor,
            width: 40,
            hyperlinks: true,
            progress: true,
            osc: OscProfile::Standard,
        }
    }

    /// A line with its SGR removed, so a test can measure what is displayed.
    fn bare(line: &str) -> String {
        let mut out = String::new();
        let mut rest = line;
        while let Some(at) = rest.find('\x1b') {
            out.push_str(&rest[..at]);
            match rest[at..].find('m') {
                Some(end) => rest = &rest[at + end + 1..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    fn drain(capabilities: Capabilities, deltas: &[&str]) -> String {
        let mut answer = Answer::new(capabilities);
        let mut out = String::new();
        for delta in deltas {
            out.push_str(&answer.push(delta));
        }
        out.push_str(&answer.finish());
        out
    }

    // Catches: delta boundaries or pipe rendering alter host marker line boundaries.
    #[test]
    fn rendered_host_markers_survive_split_deltas_and_pipes() {
        let source = "TUICommander v1.8.0 is connected.\nintent: Checking **output** (Output check)\n\nordinary\nprose\nsuggest: [ A | B | C ]\n";
        let expected = "TUICommander v1.8.0 is connected.\nintent: Checking output (Output check)\n\nordinary prose\nsuggest: [ A | B | C ]\n";
        let chars: Vec<String> = source.chars().map(|ch| ch.to_string()).collect();
        let deltas: Vec<&str> = chars.iter().map(String::as_str).collect();
        for width in [80, 120, 200] {
            let caps = Capabilities { width, ..screen() };
            assert_eq!(bare(&drain(caps, &[source])), expected);
            assert_eq!(bare(&drain(caps, &deltas)), expected);
        }
        assert_eq!(drain(Capabilities::plain(), &deltas), source);
    }

    // Catches: real PTY output inserts hard row boundaries in a marker's tail.
    #[cfg(unix)]
    #[test]
    fn rendered_host_markers_keep_pty_soft_wraps() {
        let source = format!(
            "TUICommander v1.8.0 is connected.\nintent: {} (PTY check)\nsuggest: [ A | B | C ]\n",
            "long marker text ".repeat(30)
        );
        let chars: Vec<String> = source.chars().map(|ch| ch.to_string()).collect();
        let deltas: Vec<&str> = chars.iter().map(String::as_str).collect();
        for width in [80, 120, 200] {
            for (mode, chunks) in [("whole", vec![source.as_str()]), ("split", deltas.clone())] {
                let rendered = drain(Capabilities { width, ..screen() }, &chunks);
                let bytes = pty_bytes(&rendered, width);
                let mut terminal = vt100::Parser::new(24, width as u16, 0);
                terminal.process(&bytes);
                let grid = terminal.screen();
                assert_eq!(
                    grid.contents().lines().next(),
                    Some("TUICommander v1.8.0 is connected.")
                );
                assert!(
                    grid.contents()
                        .lines()
                        .nth(1)
                        .unwrap()
                        .starts_with("intent: ")
                );
                assert!(
                    grid.row_wrapped(1),
                    "marker tail must be a terminal soft wrap"
                );
                assert!(
                    grid.contents()
                        .lines()
                        .any(|row| row == "suggest: [ A | B | C ]")
                );
                println!(
                    "PTY {mode} width={width} bytes={bytes:?} grid={:?}",
                    grid.contents()
                );
            }
        }
    }

    #[cfg(unix)]
    fn pty_bytes(rendered: &str, width: usize) -> Vec<u8> {
        use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
        use std::io::Read;

        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows: 24,
                cols: width as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "printf '%s' \"$1\"", "answer-capture", rendered]);
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("PTY read: {error}"),
            }
        }
        assert!(child.wait().unwrap().success());
        bytes
    }

    // Catches: PTY-width paragraph hard breaks and hanging indents survive narrower viewers.
    #[cfg(unix)]
    #[test]
    fn answer_paragraphs_leave_wrapping_to_the_viewer_at_221_columns() {
        let mut errors = Vec::new();
        for prefix in ["", "💬 "] {
            let source = format!(
                "{prefix}{}END-AAW",
                "L’eseguibile è stato rimosso perché il collegamento è rimasto sul percorso precedente. ".repeat(8)
            );
            let chars: Vec<String> = source.chars().map(|ch| ch.to_string()).collect();
            let split: Vec<&str> = chars.iter().map(String::as_str).collect();
            for chunks in [vec![source.as_str()], split] {
                let rendered = drain(
                    Capabilities {
                        width: 221,
                        ..screen()
                    },
                    &chunks,
                );
                let bytes = pty_bytes(&rendered, 221);
                eprintln!(
                    "PTY-AAW prefix={prefix:?} bytes={:?}",
                    String::from_utf8_lossy(&bytes)
                );
                let actual = bare(&String::from_utf8(bytes).unwrap());
                if actual != format!("{source}\r\n") {
                    errors.push(format!("paragraph has renderer hard breaks: {actual:?}"));
                }
            }
        }
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }

    /// Redirected text output remains plain Markdown rather than terminal rendering.
    #[test]
    fn a_pipe_receives_the_models_markdown_byte_for_byte() {
        let source = "# Title\n\nSome **bold** text.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let deltas: Vec<&str> = source.split_inclusive(' ').collect();
        assert_eq!(drain(Capabilities::plain(), &deltas), source);
    }

    #[test]
    fn a_screen_receives_a_rendering() {
        let out = drain(screen(), &["Some **bold** text.\n"]);
        assert!(out.contains("\x1b[1mbold\x1b[0m"), "{out:?}");
        assert!(!out.contains("**"));
    }

    /// The point of splitting at all: a finished paragraph is drawn while the
    /// next one is still arriving.
    #[test]
    fn a_finished_block_is_drawn_before_the_next_one_arrives() {
        let mut answer = Answer::new(screen());
        assert_eq!(answer.push("First paragraph."), "");
        let flushed = answer.push("\n\nSecond para");
        assert!(flushed.contains("First paragraph."), "{flushed:?}");
        assert!(
            !flushed.contains("Second"),
            "the unfinished block waits: {flushed:?}"
        );
        assert!(answer.finish().contains("Second para"));
    }

    /// The case that makes a naive "flush on blank line" wrong. A blank line
    /// *inside* a fence is not a block boundary, and flushing there renders
    /// half a code block as prose.
    #[test]
    fn a_blank_line_inside_a_fence_is_not_a_boundary() {
        let mut answer = Answer::new(screen());
        answer.push("```rust\nfn a() {}\n");
        let flushed = answer.push("\nfn b() {}\n");
        assert_eq!(flushed, "", "still inside the fence: {flushed:?}");
        let out = answer.push("```\n\n");
        assert!(out.contains("fn a()") && out.contains("fn b()"), "{out:?}");
        assert!(!out.contains("```"));
    }

    /// A table cannot be laid out until its widest cell has arrived. It must
    /// therefore never be flushed halfway.
    #[test]
    fn a_table_is_rendered_once_and_whole() {
        let source =
            "| provider | endpoint |\n|---|---|\n| a | one |\n| bb | a much longer one |\n\n";
        let out = drain(screen(), &source.split_inclusive('|').collect::<Vec<_>>());
        let rows: Vec<&str> = out.lines().filter(|line| line.contains('│')).collect();
        assert_eq!(rows.len(), 3, "header and two rows, drawn once: {out}");
        // Every rule and every row is the same width, which is only true if the
        // whole table was laid out together.
        let widths: Vec<usize> = out
            .lines()
            .map(|line| crate::render::line::cells(&bare(line)))
            .collect();
        assert_eq!(widths.len(), 6, "{out}");
        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]), "{out}");
    }

    /// However the deltas are cut up, the rendering is the same. A boundary
    /// that depended on where the provider chunked its SSE would be a
    /// rendering that changed with the network.
    #[test]
    fn the_result_does_not_depend_on_where_the_stream_was_cut() {
        let source = "# H\n\nOne **two** three.\n\n- a\n- b\n\n```rs\nlet x = 1;\n```\n\nEnd.\n";
        let whole = drain(screen(), &[source]);
        let by_char: Vec<String> = source.chars().map(|c| c.to_string()).collect();
        let by_char: Vec<&str> = by_char.iter().map(String::as_str).collect();
        assert_eq!(drain(screen(), &by_char), whole);
        let by_line: Vec<&str> = source.split_inclusive('\n').collect();
        assert_eq!(drain(screen(), &by_line), whole);
    }

    /// A blank line is only a block boundary at the **top level**. Inside a
    /// list it separates two items of one list, and flushing there renders the
    /// first half as a finished list and the continuation as something else
    /// entirely — an indented paragraph that has lost its bullet, or an ordered
    /// list that starts counting again.
    ///
    /// The rule is not "a blank line ends a block". It is "a blank line ends a
    /// block that nothing is inside".
    #[test]
    fn a_blank_line_inside_a_list_is_not_a_boundary() {
        let loose = "- first item\n\n- second item\n\n  a continuation of the second\n\n";
        let out = drain(screen(), &[loose]);
        assert_eq!(out.matches('•').count(), 2, "one list, two bullets: {out}");

        // And the same document cut into deltas renders identically, which is
        // the property a mid-list flush destroys.
        let by_line: Vec<&str> = loose.split_inclusive('\n').collect();
        assert_eq!(drain(screen(), &by_line), out);
    }

    /// The same rule for the other containers: a table row is not a block, and
    /// a blockquote that continues after a blank line is one quote.
    #[test]
    fn no_container_is_flushed_before_it_closes() {
        let quote = "> first paragraph of the quote\n>\n> second paragraph\n\nAfter.\n";
        let out = drain(screen(), &quote.split_inclusive('\n').collect::<Vec<_>>());
        let bars = out.lines().filter(|line| line.contains('▌')).count();
        assert_eq!(bars, 3, "one quote, three lines of it: {out}");
    }

    #[test]
    fn an_empty_answer_writes_nothing() {
        assert_eq!(drain(screen(), &[]), "");
        assert_eq!(drain(screen(), &["   \n\n  "]), "");
        assert_eq!(drain(Capabilities::plain(), &[]), "");
    }

    #[test]
    fn blocks_are_separated_by_one_blank_line_and_none_leads() {
        let out = drain(screen(), &["one\n\n", "two\n\n", "three\n"]);
        assert!(!out.starts_with('\n'), "{out:?}");
        assert!(!out.contains("\n\n\n"), "{out:?}");
        assert_eq!(out.matches("\n\n").count(), 2, "{out:?}");
    }
}
