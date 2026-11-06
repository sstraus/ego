//! What a local Ollama daemon puts reasoning text in, and what this
//! repository does with it. Recorded, not reasoned about.
//!
//! # Why this file exists
//!
//! Reasoning extension fields need decoding even when token accounting already reports
//! their billed output. These fixtures preserve that distinction.
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
//! 117 of those 118 output tokens were reasoning, and every one of them was
//! invisible to this repository: replaying the transcript through its own
//! decoder yielded the single character `9`, and
//! [`AiEvent::ReasoningDelta`] — which exists — never fired.
//!
//! # The remedy, and where this file was wrong about it
//!
//! Decode both reasoning extension spellings uniformly. Opportunistic
//! decoding must not require a provider-name switch or drop a spelling
//! another member of the mapping uses.
//!
//! So `Delta` reads `reasoning` and `reasoning_content`, no branch and no
//! provider id, and this row's reasoning text now reaches
//! [`AiEvent::ReasoningDelta`] — its first producer in the workspace.
//!
//! **What the fix does not recover.** The usage chunk carries no
//! `completion_tokens_details`, so the reasoning tokens still cannot be
//! separated from the answer's after the fact. They are counted, they settle
//! at the output rate, and nothing downstream can tell that 99% of the turn
//! was thinking. The text is no longer lost; the accounting still is.

use ego_ai::wire::openai_completions::StreamDecoder;
use ego_core::generator::port::AiEvent;

const HIGH_REQUEST: &str = include_str!("../goldens/ollama_reasoning_high.request.json");
const HIGH_TRANSCRIPT: &str = include_str!("../goldens/ollama_reasoning_high.sse");
const NONE_REQUEST: &str = include_str!("../goldens/ollama_reasoning_none.request.json");
const NONE_TRANSCRIPT: &str = include_str!("../goldens/ollama_reasoning_none.sse");

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

/// Everything the decoder surfaced as reasoning text.
fn reasoning_of(events: &[AiEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AiEvent::ReasoningDelta { text } => Some(text.as_str()),
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

/// Ollama uses `reasoning`; the archived xAI Chat schema uses `reasoning_content`. Decode
/// both uniformly.
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

/// The finding, now inverted: every character of reasoning on the wire
/// reaches `AiEvent::ReasoningDelta`, and none of it is confused with the
/// answer.
///
/// The assertion is equality against what the recording actually carries, not
/// "at least one event fired". A decoder that surfaced the first chunk and
/// dropped the rest would pass the weaker claim while losing most of the text,
/// which is the failure this file was written about.
#[test]
fn every_reasoning_token_reaches_an_event_of_its_own() {
    let on_the_wire = reasoning_on_the_wire(HIGH_TRANSCRIPT);
    let events = replay(HIGH_TRANSCRIPT);

    assert_eq!(
        text_of(&events),
        ANSWER,
        "the decoder surfaced something other than the visible answer"
    );
    assert_eq!(
        reasoning_of(&events),
        on_the_wire,
        "the reasoning that reached events is not the reasoning the daemon \
         sent"
    );

    // Stated as an inequality rather than a count, so the test survives a
    // re-recording: what matters is that this recording is still mostly
    // reasoning, or it demonstrates nothing either way.
    assert!(
        on_the_wire.len() > text_of(&events).len() * 100,
        "this recording no longer has enough reasoning in it to demonstrate \
         anything: {} chars of reasoning against {} of answer",
        on_the_wire.len(),
        text_of(&events).len()
    );

    // The control: no reasoning asked for, no reasoning event invented.
    assert!(
        reasoning_of(&replay(NONE_TRANSCRIPT)).is_empty(),
        "the control recording carries no delta.reasoning, so any \
         ReasoningDelta from it was invented"
    );
}

/// And they are counted, which reading them does not change. Same model, same
/// prompt, same one-character answer: 118 output tokens with reasoning on, 1
/// with it off. The usage chunk offers no `completion_tokens_details`, so the
/// 117 cannot be told apart from the answer afterwards — they settle at the
/// output rate like any other. This is the half of the finding the fix does
/// not close, and it stays pinned so nobody reads the recovered text as
/// recovered accounting.
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
