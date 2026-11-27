//! Model subscriptions to new complete output lines of a background exec.
//!
//! The pipe writer feeds every masked chunk here, the same bytes the
//! `exec_status` tail receives. Matching lines wait in a bounded queue until
//! the next generation boundary takes them.

use ego_core::event::id::DetachedExecId;
use ego_core::permit::{
    DetachedExecLines, DetachedExecMonitor, MonitoredLine, ProcessError, StreamProvenance,
};
use regex::Regex;

/// Marks a line cut at [`DetachedExecMonitor::LINE_BYTES`].
const CUT_MARKER: &str = " [line cut]";

pub(super) struct LineMonitor {
    pattern: Option<Regex>,
    max_lines: usize,
    stdout: PartialLine,
    stderr: PartialLine,
    pending: Vec<MonitoredLine>,
    pending_bytes: usize,
    omitted: u64,
}

#[derive(Default)]
struct PartialLine {
    bytes: Vec<u8>,
    cut: bool,
    /// The subscription started inside this line: its head is not new output.
    skip: bool,
}

/// Validate what the model asked for, before any registry state changes.
pub(super) fn compile(
    monitor: &DetachedExecMonitor,
) -> Result<(Option<Regex>, usize), ProcessError> {
    if !(1..=DetachedExecMonitor::MAX_LINES).contains(&monitor.max_lines) {
        return Err(ProcessError::Invariant(format!(
            "monitor max_lines must be from 1 to {}",
            DetachedExecMonitor::MAX_LINES
        )));
    }
    let pattern = monitor
        .pattern
        .as_deref()
        .map(Regex::new)
        .transpose()
        .map_err(|error| ProcessError::Invariant(format!("invalid monitor pattern: {error}")))?;
    Ok((pattern, monitor.max_lines))
}

impl LineMonitor {
    /// Start after the output seen so far: a stream whose tail has no final
    /// newline is inside a line, and that line's head is not new output.
    pub(super) fn new(
        (pattern, max_lines): (Option<Regex>, usize),
        stdout_tail: &[u8],
        stderr_tail: &[u8],
    ) -> Self {
        let inside = |tail: &[u8]| tail.last().is_some_and(|byte| *byte != b'\n');
        Self {
            pattern,
            max_lines,
            stdout: PartialLine {
                skip: inside(stdout_tail),
                ..PartialLine::default()
            },
            stderr: PartialLine {
                skip: inside(stderr_tail),
                ..PartialLine::default()
            },
            pending: Vec::new(),
            pending_bytes: 0,
            omitted: 0,
        }
    }

    /// Replace the filter and limit; queued lines and lines in progress stay.
    pub(super) fn retarget(&mut self, (pattern, max_lines): (Option<Regex>, usize)) {
        self.pattern = pattern;
        self.max_lines = max_lines;
    }

    /// Feed one masked chunk. Returns true when the queue was empty and now
    /// holds a delivery, so the host is woken once per delivery.
    pub(super) fn push(&mut self, stream: StreamProvenance, chunk: &[u8]) -> bool {
        let was_ready = self.ready();
        let mut rest = chunk;
        while !rest.is_empty() {
            let partial = match stream {
                StreamProvenance::Stdout => &mut self.stdout,
                StreamProvenance::Stderr => &mut self.stderr,
            };
            let Some(end) = rest.iter().position(|byte| *byte == b'\n') else {
                partial.extend(rest);
                break;
            };
            partial.extend(&rest[..end]);
            rest = &rest[end + 1..];
            if let Some(line) = partial.finish() {
                self.offer(stream, line);
            }
        }
        !was_ready && self.ready()
    }

    pub(super) fn ready(&self) -> bool {
        !self.pending.is_empty() || self.omitted > 0
    }

    pub(super) fn take(&mut self, id: DetachedExecId) -> Option<DetachedExecLines> {
        if !self.ready() {
            return None;
        }
        self.pending_bytes = 0;
        Some(DetachedExecLines {
            id,
            lines: std::mem::take(&mut self.pending),
            omitted: std::mem::take(&mut self.omitted),
        })
    }

    fn offer(&mut self, stream: StreamProvenance, text: String) {
        if self
            .pattern
            .as_ref()
            .is_some_and(|pattern| !pattern.is_match(&text))
        {
            return;
        }
        if self.pending.len() >= self.max_lines
            || self.pending_bytes + text.len() > DetachedExecMonitor::DELIVERY_BYTES
        {
            self.omitted = self.omitted.saturating_add(1);
            return;
        }
        self.pending_bytes += text.len();
        self.pending.push(MonitoredLine { stream, text });
    }
}

impl PartialLine {
    fn extend(&mut self, bytes: &[u8]) {
        if self.skip {
            return;
        }
        let room = DetachedExecMonitor::LINE_BYTES.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
        self.cut |= bytes.len() > room;
    }

    /// End the current line; `None` for the head of a line seen before the
    /// subscription.
    fn finish(&mut self) -> Option<String> {
        if std::mem::take(&mut self.skip) {
            return None;
        }
        let mut bytes = std::mem::take(&mut self.bytes);
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if std::mem::take(&mut self.cut) {
            text.push_str(CUT_MARKER);
        }
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(pattern: Option<&str>, max_lines: usize) -> LineMonitor {
        let request = DetachedExecMonitor {
            pattern: pattern.map(str::to_owned),
            max_lines,
        };
        LineMonitor::new(compile(&request).unwrap(), b"", b"")
    }

    fn texts(lines: &DetachedExecLines) -> Vec<&str> {
        lines.lines.iter().map(|line| line.text.as_str()).collect()
    }

    /// Catches: a line split across pipe chunks delivered as two lines, or a
    /// pattern tested against a chunk instead of the whole line.
    #[test]
    fn a_line_split_across_chunks_is_matched_once_as_one_line() {
        let mut monitor = monitor(Some("listening on [0-9]+"), 20);
        assert!(!monitor.push(StreamProvenance::Stdout, b"compiling\nlisten"));
        assert!(monitor.push(StreamProvenance::Stdout, b"ing on 3000\r\nmore"));
        let id = DetachedExecId::new();
        let lines = monitor.take(id).expect("one delivery");
        assert_eq!(texts(&lines), ["listening on 3000"]);
        assert_eq!(lines.omitted, 0);
        assert!(monitor.take(id).is_none(), "a delivery is taken once");
    }

    /// Catches: an unbounded queue, or a bound that drops the count of the
    /// lines it refused.
    #[test]
    fn a_delivery_is_bounded_in_lines_and_bytes_and_counts_the_rest() {
        let mut lines_bound = monitor(None, 2);
        lines_bound.push(StreamProvenance::Stderr, b"a\nb\nc\nd\n");
        let taken = lines_bound.take(DetachedExecId::new()).unwrap();
        assert_eq!(texts(&taken), ["a", "b"]);
        assert_eq!(taken.omitted, 2);

        let mut bytes_bound = monitor(None, DetachedExecMonitor::MAX_LINES);
        let long = format!("{}\n", "x".repeat(DetachedExecMonitor::LINE_BYTES * 3));
        for _ in 0..12 {
            bytes_bound.push(StreamProvenance::Stdout, long.as_bytes());
        }
        let taken = bytes_bound.take(DetachedExecId::new()).unwrap();
        let bytes: usize = taken.lines.iter().map(|line| line.text.len()).sum();
        assert!(
            bytes <= DetachedExecMonitor::DELIVERY_BYTES,
            "{bytes} bytes"
        );
        assert!(taken.lines[0].text.ends_with(CUT_MARKER));
        assert_eq!(taken.lines.len() as u64 + taken.omitted, 12);
    }

    /// Catches: the tail of a line already in progress at subscription time
    /// reported as a new line.
    #[test]
    fn a_subscription_inside_a_line_starts_at_the_next_line() {
        let request = DetachedExecMonitor {
            pattern: None,
            max_lines: 20,
        };
        let mut monitor = LineMonitor::new(compile(&request).unwrap(), b"old\npart", b"done\n");
        monitor.push(StreamProvenance::Stdout, b"ial\nnew\n");
        monitor.push(StreamProvenance::Stderr, b"fresh\n");
        let taken = monitor.take(DetachedExecId::new()).unwrap();
        assert_eq!(texts(&taken), ["new", "fresh"]);
        assert_eq!(taken.lines[1].stream, StreamProvenance::Stderr);
    }

    /// Catches: an invalid pattern or limit installed as a monitor that
    /// silently never delivers.
    #[test]
    fn an_invalid_request_is_refused_before_it_is_installed() {
        for (pattern, max_lines) in [(Some("("), 5), (None, 0), (None, 101)] {
            let request = DetachedExecMonitor {
                pattern: pattern.map(str::to_owned),
                max_lines,
            };
            assert!(compile(&request).is_err(), "{pattern:?} {max_lines}");
        }
    }
}
