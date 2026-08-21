//! What a local Ollama daemon does with an output ceiling, recorded rather
//! than reasoned about.
//!
//! # Why this file exists
//!
//! `docs/` claimed, in prose, that Ollama's OpenAI-compatible endpoint
//! discards `max_completion_tokens`. Prose is not replayable, and the
//! adjacent claim about `reasoning_effort` — that Ollama *validates* all
//! seven levels, when it in fact clamps two of them — had already been wrong
//! in exactly this way: a `200` was read as proof the server did what the
//! request said. It proves only that it took the request.
//!
//! So this is the measurement, committed. The goldens here are **captured
//! from a live daemon**, which makes them different in kind from
//! `goldens/stream_*.sse` — those are assembled from `openai/openai-openapi`
//! because no OpenAI account was available, and they say so. These were
//! produced by `curl` against `http://localhost:11434/v1/chat/completions` on
//! 2026-08-21, `GET /api/version` answering `{"version":"0.32.14"}`, model
//! `gemma4:e4b-mlx`, same prompt and same ceiling of 4 in both, differing in
//! exactly one JSON member.
//!
//! # What they show
//!
//! | request member | completion_tokens | finish_reason |
//! |---|---|---|
//! | `max_completion_tokens: 4` | 110 | `stop` |
//! | `max_tokens: 4` | 4 | `length` |
//!
//! The daemon honours `max_tokens` and ignores `max_completion_tokens`. Both
//! requests returned `200`.
//!
//! `crates/ego-ai/src/wire/openai_completions.rs` sends
//! `max_completion_tokens` and deliberately does not send `max_tokens`, which
//! is correct for OpenAI — `max_tokens` is deprecated there, and the wire
//! format is not the place to learn a provider's name. The consequence is
//! recorded here rather than papered over: **`GenRequest::max_output_tokens`
//! does not bind on this row.** The remedy is a mapping decision above this
//! layer, not a branch inside a shared wire format, and it is not made here.

use ego_ai::wire::openai_completions::{StreamDecoder, request_body};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    Conversation, GenRequest, Message, Role, StopReason, ThinkingLevel,
};

const IGNORED_REQUEST: &str = include_str!("goldens/ollama_ceiling_ignored.request.json");
const IGNORED_TRANSCRIPT: &str = include_str!("goldens/ollama_ceiling_ignored.sse");
const HONOURED_REQUEST: &str = include_str!("goldens/ollama_ceiling_honoured.request.json");
const HONOURED_TRANSCRIPT: &str = include_str!("goldens/ollama_ceiling_honoured.sse");

/// The ceiling both recorded requests asked for.
const CEILING: u64 = 4;

fn recorded(request: &str) -> serde_json::Value {
    serde_json::from_str(request).expect("the recorded request is valid JSON")
}

fn replay(transcript: &str) -> Vec<AiEvent> {
    let mut decoder = StreamDecoder::default();
    let mut events = decoder.push(transcript.as_bytes());
    events.extend(decoder.finish(None));
    events
}

fn output_tokens(events: &[AiEvent]) -> u64 {
    events
        .iter()
        .find_map(|e| match e {
            AiEvent::Usage(usage) => Some(usage.output_tokens),
            _ => None,
        })
        .expect("both transcripts carry a usage chunk")
}

fn stop(events: &[AiEvent]) -> StopReason {
    events
        .iter()
        .find_map(|e| match e {
            AiEvent::Stop(reason) => Some(*reason),
            _ => None,
        })
        .expect("both transcripts terminate")
}

/// The measurement itself: the same prompt and the same ceiling, asked for
/// under two different names, answered differently.
#[test]
fn ollama_honours_max_tokens_and_ignores_max_completion_tokens() {
    let ignored = recorded(IGNORED_REQUEST);
    assert_eq!(ignored["max_completion_tokens"], serde_json::json!(CEILING));
    assert!(
        ignored.get("max_tokens").is_none(),
        "the recording that shows the ceiling being ignored must not also \
         send the field that works: {ignored}"
    );

    let honoured = recorded(HONOURED_REQUEST);
    assert_eq!(honoured["max_tokens"], serde_json::json!(CEILING));
    assert!(
        honoured.get("max_completion_tokens").is_none(),
        "the recording that shows the ceiling binding must isolate the one \
         field that made it bind: {honoured}"
    );

    let overrun = output_tokens(&replay(IGNORED_TRANSCRIPT));
    assert!(
        overrun > CEILING,
        "the daemon was asked for at most {CEILING} output tokens via \
         max_completion_tokens and produced {overrun}; if this now holds, \
         Ollama has started reading the field and the mapping should be \
         revisited"
    );

    let bounded = output_tokens(&replay(HONOURED_TRANSCRIPT));
    assert_eq!(
        bounded, CEILING,
        "the daemon was asked for at most {CEILING} output tokens via \
         max_tokens and produced {bounded}"
    );
}

/// The consequence for this repository, stated as a test rather than as a
/// comment: the field `ego` sends is the field the recorded daemon ignored.
///
/// This is the guard. It fails the day anyone claims a local model's output
/// can be bounded — either because the wire format started sending
/// `max_tokens`, or because someone edited the recording.
#[test]
fn the_ceiling_ego_sends_is_the_one_this_daemon_threw_away() {
    let req = GenRequest {
        model: "gemma4:e4b-mlx".to_string(),
        messages: Conversation::new(vec![Message::text(Role::User, "Count from 1 to 40.")])
            .expect("a one-message history is valid"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Low,
        max_output_tokens: Some(CEILING),
        parallel_tool_calls: false,
        reasoning_state: None,
    };
    let body: serde_json::Value =
        serde_json::from_slice(&request_body(&req).expect("this request is expressible"))
            .expect("the body is valid JSON");

    assert_eq!(
        body["max_completion_tokens"],
        serde_json::json!(CEILING),
        "the wire format stopped sending max_completion_tokens: {body}"
    );
    assert!(
        body.get("max_tokens").is_none(),
        "the wire format has started sending max_tokens; that is the field \
         Ollama honours, but it is deprecated for OpenAI, so this is a \
         mapping decision and not a field to add quietly: {body}"
    );

    // The join: what we send is what the recording proves is discarded.
    let discarded = recorded(IGNORED_REQUEST);
    assert_eq!(
        body["max_completion_tokens"], discarded["max_completion_tokens"],
        "the recording no longer exercises the field this format sends, so it \
         no longer says anything about our requests"
    );
}

/// The half of the survey's open question that turned out **not** to be a
/// defect, pinned so it stays that way.
///
/// A bounded Ollama generation terminates with `finish_reason: "length"`,
/// which is one of the five spellings `consume_finish_reason` knows. It maps
/// to [`StopReason::Length`] and emits no [`AiEvent::Error`] — so an ordinary
/// bounded generation is *not* logged as a failure. Had Ollama spelled it
/// anything else, it would have been.
#[test]
fn a_bounded_ollama_generation_is_length_and_not_an_error() {
    let events = replay(HONOURED_TRANSCRIPT);
    assert_eq!(stop(&events), StopReason::Length);
    assert!(
        !events.iter().any(|e| matches!(e, AiEvent::Error { .. })),
        "a generation that merely hit its ceiling was reported as an error: {events:?}"
    );
}

/// The unbounded control, from the same daemon and the same prompt: it ends
/// at `stop`, so the `length` above is the ceiling talking and not something
/// this model does on every turn.
#[test]
fn the_generation_that_was_not_bounded_ends_at_stop() {
    let events = replay(IGNORED_TRANSCRIPT);
    assert_eq!(stop(&events), StopReason::Stop);
}
