//! What terminal `finish_reason` a local Ollama daemon puts on a turn that
//! called a tool. Recorded, not reasoned about.
//! Ollama tool-call recordings exercise terminal `tool_calls` frames with
//! and without reasoning. Captured from a local 0.32.14 daemon serving
//! `gemma4:12b-mlx`; both recordings must decode as tool calls, not errors.
//! Response/tool identities and clocks are synthetic; observed payloads are preserved.

use ego_ai::wire::openai_completions::StreamDecoder;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::StopReason;

const PLAIN_REQUEST: &str = include_str!("goldens/ollama_tool_call.request.json");
const PLAIN_TRANSCRIPT: &str = include_str!("goldens/ollama_tool_call.sse");
const THINKING_REQUEST: &str = include_str!("goldens/ollama_tool_call_thinking.request.json");
const THINKING_TRANSCRIPT: &str = include_str!("goldens/ollama_tool_call_thinking.sse");

/// The tool both recordings offered, and the one both called.
const TOOL: &str = "read_file";

fn replay(transcript: &str) -> Vec<AiEvent> {
    let mut decoder = StreamDecoder::default();
    let mut events = decoder.push(transcript.as_bytes());
    events.extend(decoder.finish(None));
    events
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

/// The `finish_reason` values present on the wire, in order.
fn finish_reasons(transcript: &str) -> Vec<String> {
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
                .filter_map(|choice| choice["finish_reason"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The measurement item 3 asked for. If Ollama ever changes this spelling,
/// this is the test that says so before a user sees a successful turn logged
/// as an error.
#[test]
fn ollama_ends_a_tool_calling_turn_with_the_spelling_tool_calls() {
    assert_eq!(finish_reasons(PLAIN_TRANSCRIPT), vec!["tool_calls"]);
    assert_eq!(finish_reasons(THINKING_TRANSCRIPT), vec!["tool_calls"]);
}

/// And it maps: through this repository's own decoder the turn is
/// [`StopReason::ToolUse`], not [`StopReason::Error`], and no error event is
/// emitted alongside it.
#[test]
fn a_tool_calling_turn_is_tool_use_and_not_an_error() {
    for (name, transcript) in [
        ("plain", PLAIN_TRANSCRIPT),
        ("thinking", THINKING_TRANSCRIPT),
    ] {
        let events = replay(transcript);
        assert_eq!(
            stop(&events),
            StopReason::ToolUse,
            "the {name} recording did not settle as a tool call"
        );
        assert!(
            !events.iter().any(|e| matches!(e, AiEvent::Error { .. })),
            "an ordinary tool-calling turn was reported as an error ({name}): {events:?}"
        );
    }
}

/// The call itself survives the trip, arguments assembled. Without this the
/// test above would still pass on a transcript that terminated correctly and
/// carried nothing to dispatch.
#[test]
fn the_call_arrives_whole_with_its_arguments() {
    for (name, transcript) in [
        ("plain", PLAIN_TRANSCRIPT),
        ("thinking", THINKING_TRANSCRIPT),
    ] {
        let events = replay(transcript);
        let call = events
            .iter()
            .find_map(|e| match e {
                AiEvent::ToolCall {
                    call_id,
                    name,
                    args,
                } => Some((call_id, name, args)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the {name} recording produced no tool call: {events:?}"));

        let (call_id, called, args) = call;
        assert_eq!(called, TOOL, "{name}: the wrong tool was called");
        assert!(
            !call_id.is_empty(),
            "{name}: a call with no id could never have its result attributed back"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(args).unwrap_or_else(|e| panic!("{name}: args are not JSON: {e}"));
        assert!(
            parsed["path"].is_string(),
            "{name}: the assembled arguments lost the required member: {args}"
        );
    }
}

/// Each recording offered exactly one tool and asked for it by name, so the
/// terminal frame above is attributable to a tool call and not to some other
/// way the turn might have ended.
#[test]
fn the_recordings_actually_offered_the_tool_they_called() {
    for (name, request) in [("plain", PLAIN_REQUEST), ("thinking", THINKING_REQUEST)] {
        let request: serde_json::Value =
            serde_json::from_str(request).expect("the recorded request is valid JSON");
        let tools = request["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("the {name} request offers no tools"));
        assert_eq!(
            tools.len(),
            1,
            "{name}: more than one tool muddies the attribution"
        );
        assert_eq!(tools[0]["function"]["name"], serde_json::json!(TOOL));
    }
}

/// And the pair is a controlled comparison: `reasoning_effort` is the only
/// thing that differs between the two requests, so the identical
/// `finish_reason` across them is a property of the tool call and not of two
/// unrelated turns that happened to agree.
#[test]
fn the_two_recordings_differ_only_in_reasoning_effort() {
    let blanked = |raw: &str| {
        let mut request: serde_json::Value =
            serde_json::from_str(raw).expect("the recorded request is valid JSON");
        request["reasoning_effort"] = serde_json::Value::Null;
        request
    };
    assert_eq!(blanked(PLAIN_REQUEST), blanked(THINKING_REQUEST));

    let effort_of = |raw: &str| {
        serde_json::from_str::<serde_json::Value>(raw).expect("valid JSON")["reasoning_effort"]
            .as_str()
            .expect("both requests name an effort")
            .to_owned()
    };
    assert_eq!(effort_of(PLAIN_REQUEST), "none");
    assert_eq!(effort_of(THINKING_REQUEST), "high");
}
