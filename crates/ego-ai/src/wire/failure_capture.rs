//! Opt-in capture of a provider failure that arrives inside an HTTP 200 stream.
//!
//! The session store keeps decoded events, not wire bytes, so a refusal that a
//! provider streams after status 200 cannot be replayed exactly from a session.
//! This capture records the status, the response headers and the stream frames
//! of such a response, once, when the stream ends in an error.
//!
//! The target is under `ego::body`, which every filter `ego-cli` builds turns
//! off. An operator turns on this capture alone with
//! `EGO_LOG=ego::body::provider_failure=debug` (and `EGO_LOG_FILE` in an
//! interactive session). The record is redacted before it is written: header
//! values outside a public allowlist, and JSON values under keys that
//! can name a credential or an account, never reach the log.

use ego_core::generator::port::AiEvent;
use ego_core::permit::HttpHeader;
use serde_json::Value;

pub(super) const TARGET: &str = "ego::body::provider_failure";

/// The most stream bytes kept. The tail is kept, because a provider sends its
/// failure frames last.
const MAX_FRAMES: usize = 64 * 1024;

const REDACTED: &str = "[redacted]";

/// Response headers whose values describe the response and never the caller.
const PUBLIC_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "date",
    "retry-after",
    "retry-after-ms",
    "server",
    "x-request-id",
];

/// Fragments of a JSON key whose value can be a credential, an account or a
/// person. Any value under such a key is replaced, whatever its type: a
/// numeric account id or a nested account object is no safer than a string.
const SECRET_KEY_PARTS: &[&str] = &[
    "token",
    "key",
    "secret",
    "auth",
    "account",
    "user",
    "email",
    "session",
    "password",
    "credential",
];

pub(super) struct FailureCapture {
    status: u16,
    headers: Vec<(String, String)>,
    frames: Vec<u8>,
    dropped: usize,
}

impl FailureCapture {
    /// A capture for this response, when the target is enabled and the status
    /// is 200. Any other status already reports its own body.
    pub(super) fn start(status: u16, headers: &[HttpHeader]) -> Option<Self> {
        if status != 200 || !tracing::enabled!(target: TARGET, tracing::Level::DEBUG) {
            return None;
        }
        Some(Self::new(status, headers))
    }

    pub(super) fn new(status: u16, headers: &[HttpHeader]) -> Self {
        let headers = headers
            .iter()
            .map(|header| {
                let name = header.name().to_ascii_lowercase();
                let public = !header.is_secret()
                    && (PUBLIC_HEADERS.contains(&name.as_str())
                        || name.starts_with("x-ratelimit-"));
                let value = if public {
                    String::from_utf8_lossy(header.value()).into_owned()
                } else {
                    REDACTED.to_string()
                };
                (name, value)
            })
            .collect();
        Self {
            status,
            headers,
            frames: Vec::new(),
            dropped: 0,
        }
    }

    pub(super) fn observe(&mut self, bytes: &[u8]) {
        self.frames.extend_from_slice(bytes);
        if self.frames.len() > MAX_FRAMES {
            let excess = self.frames.len() - MAX_FRAMES;
            self.frames.drain(..excess);
            self.dropped += excess;
        }
    }

    /// Write the record when these events end the stream in an error: a
    /// provider failure frame, or a stream that broke off. Returns the capture
    /// back while the stream has not failed.
    pub(super) fn settle(self, events: &[AiEvent]) -> Option<Self> {
        if !events
            .iter()
            .any(|event| matches!(event, AiEvent::Error { .. }))
        {
            return Some(self);
        }
        tracing::debug!(target: TARGET, capture = %self.record(), "an HTTP 200 stream ended in an error");
        None
    }

    /// The redacted record: status, headers, then the stream lines.
    pub(super) fn record(&self) -> String {
        let mut out = format!("status: {}\n", self.status);
        for (name, value) in &self.headers {
            out.push_str(&format!("header {name}: {value}\n"));
        }
        if self.dropped > 0 {
            out.push_str(&format!("frames: first {} bytes dropped\n", self.dropped));
        }
        out.push_str("frames:\n");
        for line in String::from_utf8_lossy(&self.frames).lines() {
            out.push_str(&redact_line(line));
            out.push('\n');
        }
        out
    }
}

/// Keep SSE structure, redact JSON data, and drop every other payload.
fn redact_line(line: &str) -> String {
    let Some(data) = line.strip_prefix("data:") else {
        let structural = line.is_empty()
            || line.starts_with(':')
            || line.starts_with("event:")
            || line.starts_with("id:")
            || line.starts_with("retry:");
        return if structural {
            line.to_string()
        } else {
            REDACTED.to_string()
        };
    };
    let data = data.trim_start();
    if data == "[DONE]" {
        return line.to_string();
    }
    match serde_json::from_str::<Value>(data) {
        Ok(mut value) => {
            redact_value(&mut value);
            format!("data: {value}")
        }
        Err(_) => format!("data: {REDACTED}"),
    }
}

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                let key = key.to_ascii_lowercase();
                if SECRET_KEY_PARTS.iter().any(|part| key.contains(part)) {
                    *value = Value::String(REDACTED.to_string());
                } else {
                    redact_value(value);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_value),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, value: &str) -> HttpHeader {
        HttpHeader::new(name, value.as_bytes().to_vec()).unwrap()
    }

    // Catches: a credential-bearing or account-bearing header value, or a
    // secret-like JSON string, reaching the log in the capture record.
    #[test]
    fn the_record_keeps_wire_shape_and_drops_secrets() {
        let mut capture = FailureCapture::new(
            200,
            &[
                header("Content-Type", "text/event-stream"),
                header("retry-after", "7"),
                header("x-ratelimit-remaining-requests", "0"),
                header("set-cookie", "sid=private-cookie"),
                header("x-account-id", "acct-private"),
            ],
        );
        capture
            .observe(b"event: error\ndata: {\"type\":\"error\",\"code\":\"server_is_overloaded\",");
        capture.observe(
            b"\"message\":\"at capacity\",\"user\":\"acct-private\",\"input_tokens\":3}\n\n",
        );
        capture.observe(b"data: not json private-text\nunexpected private-line\ndata: [DONE]\n");

        let record = capture.record();
        assert_eq!(
            record,
            "status: 200\n\
             header content-type: text/event-stream\n\
             header retry-after: 7\n\
             header x-ratelimit-remaining-requests: 0\n\
             header set-cookie: [redacted]\n\
             header x-account-id: [redacted]\n\
             frames:\n\
             event: error\n\
             data: {\"type\":\"error\",\"code\":\"server_is_overloaded\",\"message\":\"at capacity\",\"user\":\"[redacted]\",\"input_tokens\":\"[redacted]\"}\n\
             \n\
             data: [redacted]\n\
             [redacted]\n\
             data: [DONE]\n"
        );
        assert!(!record.contains("private"), "{record}");
    }

    // Catches: a numeric account id or a nested account object passing the
    // redaction because only string values were replaced.
    #[test]
    fn non_string_values_under_secret_keys_are_redacted() {
        let mut capture = FailureCapture::new(200, &[]);
        capture.observe(
            b"data: {\"account_id\":4242424242,\"account\":{\"name\":\"private-name\",\"tier\":7}}\n",
        );
        let record = capture.record();
        assert!(
            record.contains("data: {\"account_id\":\"[redacted]\",\"account\":\"[redacted]\"}"),
            "{record}"
        );
        assert!(!record.contains("4242424242"), "{record}");
        assert!(!record.contains("private-name"), "{record}");
    }

    // Catches: a long stream pushing the failure frames, which come last, out
    // of the bounded capture.
    #[test]
    fn a_long_stream_keeps_its_last_frames() {
        let mut capture = FailureCapture::new(200, &[]);
        capture.observe(&vec![b' '; MAX_FRAMES]);
        capture.observe(b"\ndata: {\"type\":\"response.failed\"}\n");
        let record = capture.record();
        assert!(
            record.contains("data: {\"type\":\"response.failed\"}"),
            "{record}"
        );
        assert!(record.contains("bytes dropped"), "{record}");
    }
}
