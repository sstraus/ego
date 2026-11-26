//! Bounds on the argument text of one model-issued tool call.
//!
//! Two different protections live here. A *degenerate* argument (a long run of
//! whitespace or of one repeated short unit) is a provider/model malfunction:
//! the generation is aborted as a retryable failure and the flood is never
//! journaled. An *oversize* argument that is not degenerate is a mistaken call:
//! it is refused with a model-visible error and journaled only as a bounded
//! prefix. The replay bound for what is already journaled is
//! `generator::history::bounded_replay_args`.

/// Largest argument text a tool call may carry and still be dispatched.
pub(super) const MAX_TOOL_CALL_ARGS_BYTES: usize = 256 * 1024;

/// Consecutive whitespace characters that no real argument contains.
const WHITESPACE_RUN_LIMIT: usize = 2_048;

/// Length of a short unit repeated back to back that no real argument contains.
const REPEAT_RUN_LIMIT: usize = 8_192;

/// Longest repeated unit the guard recognises.
const MAX_REPEAT_PERIOD: usize = 8;

/// Why this argument text is a degenerate flood, if it is one.
pub(super) fn degenerate_cause(args: &str) -> Option<&'static str> {
    if args.len() < WHITESPACE_RUN_LIMIT {
        return None;
    }
    let mut run = 0_usize;
    for byte in args.bytes() {
        if matches!(byte, b' ' | b'\t' | b'\n' | b'\r') {
            run += 1;
            if run >= WHITESPACE_RUN_LIMIT {
                return Some("a long run of whitespace");
            }
        } else {
            run = 0;
        }
    }
    if args.len() < REPEAT_RUN_LIMIT {
        return None;
    }
    let bytes = args.as_bytes();
    for period in 1..=MAX_REPEAT_PERIOD {
        let mut matching = 0_usize;
        for index in period..bytes.len() {
            if bytes[index] == bytes[index - period] {
                matching += 1;
                if matching >= REPEAT_RUN_LIMIT {
                    return Some("a long repetition of one short unit");
                }
            } else {
                matching = 0;
            }
        }
    }
    None
}

/// The model-visible error for an accepted-shape call whose arguments exceed
/// [`MAX_TOOL_CALL_ARGS_BYTES`].
pub(super) fn oversize_refusal(name: &str, length: usize) -> String {
    format!(
        "tool call `{name}` was not run: its arguments are {length} bytes, over the \
         {MAX_TOOL_CALL_ARGS_BYTES}-byte limit, so nothing was changed. Sending the same call \
         again will be refused again. Send smaller arguments: split the content over several \
         calls (for a file, write the first part and append the rest with later calls) or \
         point at data already on disk instead of inlining it."
    )
}

/// The journaled, retryable explanation for an aborted degenerate generation.
pub(super) fn degenerate_message(name: &str, length: usize, cause: &str) -> String {
    format!(
        "the provider streamed arguments for tool call `{name}` that are {length} bytes of \
         {cause}; the generation was aborted, the call was not stored or run, and the request \
         can be retried"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: a whitespace flood in tool arguments being accepted as a call.
    #[test]
    fn whitespace_flood_is_degenerate_even_between_json_tokens() {
        let args = format!("{{\"path\":\"a\",{}\"x\":1}}", "\t ".repeat(3_000));
        assert!(degenerate_cause(&args).is_some());
    }

    // Adjacent: the same bytes of a pretty-printed argument are fine.
    #[test]
    fn pretty_printed_arguments_are_not_degenerate() {
        let args = format!(
            "{{\n    \"path\": \"a\",\n    \"text\": \"{}\"\n}}",
            "line\n".repeat(500)
        );
        assert!(degenerate_cause(&args).is_none());
    }

    // Adjacent: a repeated short unit that is not whitespace is caught too.
    #[test]
    fn repeated_unit_flood_is_degenerate() {
        let args = format!("{{\"t\":\"{}\"}}", "ab".repeat(9_000));
        assert!(degenerate_cause(&args).is_some());
    }

    // Adjacent: large but varied content is not a flood.
    #[test]
    fn large_varied_arguments_are_not_degenerate() {
        let body: String = (0..40_000)
            .map(|i| char::from(b'a' + (i * 7 % 26) as u8))
            .collect();
        assert!(degenerate_cause(&format!("{{\"t\":\"{body}\"}}")).is_none());
    }

    // Adjacent: the refusal says operation, cause, retry advice and corrected shape.
    #[test]
    fn oversize_refusal_names_cause_retry_and_shape() {
        let text = oversize_refusal("write", 300_000);
        assert!(text.contains("`write`") && text.contains("300000"));
        assert!(text.contains("refused again") && text.contains("split"));
    }
}
