//! What a local Ollama daemon puts reasoning text in, and what this
//! repository does with it. Recorded, not reasoned about.
//!
//! # Why this file exists
//!
//! `POTENTIALS.md` P24 records that xAI streams reasoning in
//! `delta.reasoning_content`, that `Delta`
//! (`crates/ego-ai/src/wire/openai_completions.rs:379`) has only `content`,
//! `refusal` and `tool_calls`, and that the field is therefore dropped while
//! its tokens are still billed at the output rate. That was a documented
//! *risk*, read from xAI's schema.
//!
//! It is now a measured fact on a second row, and the spelling is not even
//! the same one. **Ollama uses `delta.reasoning`, not
//! `delta.reasoning_content`.** So there are two providers, two different
//! extension fields for the same concept, and neither is read.
//!
//! # The recordings
//!
//! Observed from `http://localhost:11434/v1/chat/completions`; response/tool identities and clocks are synthetic,
//! `GET /api/version` answering `{"version":"0.32.14"}`, model
//! `gemma4:12b-mlx`. Two requests, **identical in every member except
//! `reasoning_effort`**, and they produce the same visible answer:
//!
//! | `reasoning_effort` | `delta.reasoning` | visible answer | `completion_tokens` |
//! |---|---|---|---|
//! | `"high"` | 351 chars | `9` | **118** |
//! | `"none"` | absent | `9` | **1** |
//!
//! 117 of those 118 output tokens are reasoning. Every one of them is
//! invisible to this repository: replaying the transcript through its own
//! decoder yields the single character `9`, and
//! [`AiEvent::ReasoningDelta`] — which exists — never fires.
//!
//! The usage chunk carries no `completion_tokens_details`, so the reasoning
//! tokens cannot be separated from the answer's afterwards either. They are
//! counted, they settle at the output rate, and nothing downstream can tell
//! that 99% of the turn was thinking.
//!
//! # Where the remedy is not
//!
//! Not in `Delta`. `openai_completions.rs`'s module doc already rules that
//! providers extending the shape "are a different row's problem, not a field
//! this one may guess at", and adding `reasoning` to the shared decoder would
//! put one provider's extension into the format every provider uses — and
//! would still miss xAI, which spells it differently. The event vocabulary
//! itself is under review in its own ADR. **This file measures and records;
//! it does not propose the shape of the fix.**

use ego_ai::wire::openai_completions::StreamDecoder;
use ego_core::generator::port::AiEvent;

const HIGH_REQUEST: &str = include_str!("goldens/ollama_reasoning_high.request.json");
const HIGH_TRANSCRIPT: &str = include_str!("goldens/ollama_reasoning_high.sse");
const NONE_REQUEST: &str = include_str!("goldens/ollama_reasoning_none.request.json");
const NONE_TRANSCRIPT: &str = include_str!("goldens/ollama_reasoning_none.sse");

/// The answer both recordings produced, reasoning or not.
const ANSWER: &str = "9";

fn recorded(request: &str) -> serde_json::Value {
    serde_json::from_str(request).expect("the recorded request is valid JSON")
}

fn replay(transcript: &str) -> Vec<AiEvent> {
    let mut decoder = StreamDecoder::default();
    let mut events = decoder.push(transcript.as_bytes());
    events.extend(decoder.finish(None));
    events
}

/// Everything the decoder surfaced as visible assistant text.
fn text_of(events: &[AiEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AiEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
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

/// Straight off the wire: how much reasoning text the recording contains, and
/// under which member name.
fn reasoning_on_the_wire(transcript: &str) -> String {
    transcript
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| *line != "[DONE]")
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .flat_map(|chunk| {
            chunk["choices"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|choice| choice["delta"]["reasoning"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The two recordings differ in exactly one request member, so the difference
/// in what came back is attributable to it and to nothing else.
#[test]
fn the_two_recordings_differ_only_in_reasoning_effort() {
    let mut high = recorded(HIGH_REQUEST);
    let mut none = recorded(NONE_REQUEST);

    assert_eq!(high["reasoning_effort"], serde_json::json!("high"));
    assert_eq!(none["reasoning_effort"], serde_json::json!("none"));

    // Blank the one member that is allowed to differ; everything else must
    // then be identical, including the prompt and the model.
    high["reasoning_effort"] = serde_json::Value::Null;
    none["reasoning_effort"] = serde_json::Value::Null;
    assert_eq!(
        high, none,
        "the recordings differ in more than reasoning_effort, so they cannot \
         attribute anything to it"
    );
}

/// Ollama spells it `reasoning`. xAI, per `POTENTIALS.md` P24, spells it
/// `reasoning_content`. Two rows, two names, one concept — which is the
/// reason this cannot be fixed by adding a field to the shared decoder.
#[test]
fn ollama_streams_reasoning_under_its_own_member_name() {
    let reasoning = reasoning_on_the_wire(HIGH_TRANSCRIPT);
    assert!(
        !reasoning.is_empty(),
        "the high-effort recording carries no delta.reasoning at all"
    );
    assert!(
        !HIGH_TRANSCRIPT.contains("reasoning_content"),
        "this recording uses xAI's spelling, so it is no longer evidence \
         that the two rows differ"
    );

    let control = reasoning_on_the_wire(NONE_TRANSCRIPT);
    assert!(
        control.is_empty(),
        "the control asked for no reasoning and got {} chars of it",
        control.len()
    );
}

/// The finding. Every character of reasoning is dropped, and
/// `AiEvent::ReasoningDelta` never fires for this row.
#[test]
fn every_reasoning_token_is_dropped_on_the_way_through() {
    let on_the_wire = reasoning_on_the_wire(HIGH_TRANSCRIPT);
    let events = replay(HIGH_TRANSCRIPT);

    assert_eq!(
        text_of(&events),
        ANSWER,
        "the decoder surfaced something other than the visible answer"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AiEvent::ReasoningDelta { .. })),
        "ReasoningDelta fired for an Ollama row; if this now holds, the \
         mapping changed and P24 should be revisited: {events:?}"
    );

    // Stated as an inequality rather than a count, so the test survives a
    // re-recording: what matters is that the reasoning text never reaches
    // any event, not that it was exactly this long.
    assert!(
        on_the_wire.len() > text_of(&events).len() * 100,
        "this recording no longer has enough reasoning in it to demonstrate \
         the loss: {} chars of reasoning against {} of answer",
        on_the_wire.len(),
        text_of(&events).len()
    );
}

/// And they were counted. Same model, same prompt, same one-character answer:
/// 118 output tokens with reasoning on, 1 with it off. The usage chunk offers
/// no `completion_tokens_details`, so the 117 cannot be told apart from the
/// answer afterwards — they settle at the output rate like any other.
#[test]
fn the_dropped_reasoning_is_still_counted_as_output() {
    let with = output_tokens(&replay(HIGH_TRANSCRIPT));
    let without = output_tokens(&replay(NONE_TRANSCRIPT));

    assert_eq!(text_of(&replay(HIGH_TRANSCRIPT)), ANSWER);
    assert_eq!(text_of(&replay(NONE_TRANSCRIPT)), ANSWER);
    assert!(
        with > without * 50,
        "the same answer cost {with} output tokens with reasoning and \
         {without} without; the gap is what is being paid for and never seen"
    );

    assert!(
        !HIGH_TRANSCRIPT.contains("completion_tokens_details"),
        "the daemon has started breaking reasoning tokens out of the \
         completion total, which would make them separable after the fact"
    );
}
