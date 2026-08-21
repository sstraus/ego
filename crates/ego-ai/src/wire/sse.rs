//! Server-sent events, decoded incrementally from arbitrary byte chunks.
//!
//! Split out from the wire format because the two fail differently and only
//! one of them is about OpenAI. Framing errors are transport-shaped — a
//! chunk boundary in the middle of a `data:` line, a CRLF split across two
//! reads, a connection that dies before the blank line — and every
//! `text/event-stream` provider has them. Field semantics are the wire
//! format's problem.
//!
//! Only the `data` field is decoded. The `event`, `id` and `retry` fields of
//! the WHATWG event-stream grammar are parsed far enough to be skipped
//! rather than mistaken for data; chat completions uses none of them, and
//! silently treating an unknown field's value as a payload would be worse
//! than ignoring it.

/// An incremental event-stream decoder.
///
/// Feed it whatever bytes arrive. It yields one `String` per dispatched
/// event, and remembers the tail that has not become one yet — which is what
/// makes "the stream stopped mid-frame" a question that can be answered, and
/// therefore the difference between a truncation and an ending.
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Bytes that have not yet formed a complete line.
    pending: Vec<u8>,
    /// `data:` values seen since the last dispatch, joined by newlines as
    /// the grammar requires.
    data: String,
    /// Whether the current event has any field at all. Distinguishes a
    /// blank line that ends an event from a blank line between events — the
    /// grammar says only the former dispatches.
    started: bool,
}

impl SseDecoder {
    /// Feed a chunk; get every event that completed within it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(line) = self.take_line() {
            match self.consume(&line) {
                Some(event) => events.push(event),
                None => continue,
            }
        }
        events
    }

    /// Whether bytes are still buffered that never became an event.
    ///
    /// After the byte stream ends this is the truncation test: leftovers
    /// mean the provider stopped in the middle of saying something, which is
    /// not the same as having finished.
    pub fn has_partial_frame(&self) -> bool {
        !self.pending.is_empty() || self.started
    }

    /// Removes the next complete line, or `None` if one has not arrived.
    ///
    /// A lone trailing `\r` is *not* a complete line: it may be the first
    /// half of a CRLF that the next chunk finishes, and treating it as a
    /// terminator would dispatch an event one read early.
    fn take_line(&mut self) -> Option<Vec<u8>> {
        let at = self
            .pending
            .iter()
            .position(|b| *b == b'\n' || *b == b'\r')?;
        let terminator = self.pending[at];
        if terminator == b'\r' {
            match self.pending.get(at + 1) {
                None => return None,
                Some(b'\n') => {
                    let line = self.pending[..at].to_vec();
                    self.pending.drain(..at + 2);
                    return Some(line);
                }
                Some(_) => {}
            }
        }
        let line = self.pending[..at].to_vec();
        self.pending.drain(..at + 1);
        Some(line)
    }

    /// Applies one line, returning an event if the line dispatched it.
    fn consume(&mut self, line: &[u8]) -> Option<String> {
        // Lines are decoded lossily on purpose. A complete line cannot split
        // a UTF-8 character (the terminators are ASCII), so invalid bytes
        // here are the provider's, and losing one character is better than
        // discarding a frame that carries a whole tool call.
        let line = String::from_utf8_lossy(line);
        if line.is_empty() {
            if !self.started {
                return None;
            }
            self.started = false;
            return Some(std::mem::take(&mut self.data));
        }
        if line.starts_with(':') {
            // A comment. Providers send these as keep-alives.
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
        };
        self.started = true;
        if field == "data" {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(value);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(decoder: &mut SseDecoder, chunks: &[&str]) -> Vec<String> {
        chunks
            .iter()
            .flat_map(|chunk| decoder.push(chunk.as_bytes()))
            .collect()
    }

    #[test]
    fn a_frame_split_across_chunks_still_arrives_whole() {
        let mut decoder = SseDecoder::default();
        // Split inside the JSON payload, inside the field name, and between
        // the two bytes of a CRLF — the three places a naive decoder breaks.
        let events = all(
            &mut decoder,
            &["da", "ta: {\"a\":", "1}\r", "\n\r\n", "data: {\"b\":2}\n\n"],
        );
        assert_eq!(
            events,
            vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]
        );
        assert!(!decoder.has_partial_frame());
    }

    /// The CRLF split the case above cannot see, and the reason it cannot.
    ///
    /// When a chunk ends in `\r`, a decoder that treats it as a terminator
    /// then reads the `\n` that follows as an *empty* line — and an empty line
    /// dispatches. Above, that lands exactly where an event was ending anyway,
    /// so the mistake cancels itself out and the test passes on a broken
    /// decoder. Here the `\n` is followed by more data, so the premature
    /// terminator splits one event into two. The `\r` has to be held until the
    /// next byte says what it was.
    #[test]
    fn a_cr_at_the_end_of_a_chunk_is_not_yet_a_line() {
        let mut decoder = SseDecoder::default();
        assert_eq!(
            all(&mut decoder, &["data: a\r", "\ndata: b\r\n\r\n"]),
            vec!["a\nb".to_string()],
            "one event, whose two data lines were split by a CRLF across chunks"
        );
    }

    #[test]
    fn a_stream_that_stops_mid_frame_says_so() {
        let mut decoder = SseDecoder::default();
        assert!(all(&mut decoder, &["data: {\"a\":1}\n\ndata: {\"b\":"]).len() == 1);
        assert!(
            decoder.has_partial_frame(),
            "the second frame never dispatched, and the decoder has to be able to say that"
        );
    }

    /// A dispatched event whose last line arrived is complete even though no
    /// blank line follows *yet* — but until that blank line arrives it has
    /// not dispatched, and reporting it as finished would invent a frame.
    #[test]
    fn a_complete_line_without_its_blank_line_has_not_dispatched() {
        let mut decoder = SseDecoder::default();
        assert!(all(&mut decoder, &["data: {\"a\":1}\n"]).is_empty());
        assert!(decoder.has_partial_frame());
        assert_eq!(all(&mut decoder, &["\n"]), vec!["{\"a\":1}".to_string()]);
        assert!(!decoder.has_partial_frame());
    }

    #[test]
    fn comments_and_unknown_fields_are_skipped_not_read_as_data() {
        let mut decoder = SseDecoder::default();
        let events = all(
            &mut decoder,
            &[
                ": keep-alive\n",
                "event: message\n",
                "data: payload\n",
                "\n",
            ],
        );
        assert_eq!(events, vec!["payload".to_string()]);
    }

    /// The grammar joins repeated `data:` lines with newlines. Chat
    /// completions never does this, but a proxy that reformats JSON can, and
    /// concatenating without the newline would corrupt it silently.
    #[test]
    fn repeated_data_lines_join_with_newlines() {
        let mut decoder = SseDecoder::default();
        assert_eq!(
            all(
                &mut decoder,
                &["data: {\n", "data: \"a\": 1\n", "data: }\n\n"]
            ),
            vec!["{\n\"a\": 1\n}".to_string()]
        );
    }

    /// Only one space after the colon is part of the framing; the rest is
    /// payload. Trimming further would corrupt data that begins with spaces.
    #[test]
    fn exactly_one_leading_space_is_framing() {
        let mut decoder = SseDecoder::default();
        assert_eq!(
            all(&mut decoder, &["data:  padded\n\n"]),
            vec![" padded".to_string()]
        );
    }
}
