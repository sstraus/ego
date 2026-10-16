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
//! format is not the place to learn a provider's name.
//!
//! # What was done about it
//!
//! For a while the consequence was only recorded: `GenRequest::max_output_tokens`
//! did not bind on this row, and the remedy was named as a mapping decision
//! above this layer. `docs/decisions/`
//! `2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §8 makes it, and
//! makes it the blunt way: a member that fails a conformance check is not a
//! member. So the Ollama row names `wire::ollama`, which shares the family's
//! request projection and its whole decoder and sends the ceiling in the field
//! measured above. Nothing is handed a flag; the profile picks a mapping.
//!
//! A **third** recording establishes that, and it is the one that matters:
//! `ollama_ceiling_adapted.*` is the exact body
//! `ProviderRow::ollama_default()` builds, posted unmodified. The first two
//! prove a field works; only this one proves *our request* does.

use ego_ai::http::{HttpError, HttpResponse};
use ego_ai::provider::ProviderRow;
use ego_ai::wire::WireFormat;
use ego_ai::wire::ollama::Ollama;
use ego_ai::wire::openai_completions;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{
    Conversation, GenRequest, Message, Role, StopReason, ThinkingLevel,
};
use ego_core::permit::HttpHeader;
use futures::StreamExt;

const IGNORED_REQUEST: &str = include_str!("goldens/ollama_ceiling_ignored.request.json");
const IGNORED_TRANSCRIPT: &str = include_str!("goldens/ollama_ceiling_ignored.sse");
const HONOURED_REQUEST: &str = include_str!("goldens/ollama_ceiling_honoured.request.json");
const HONOURED_TRANSCRIPT: &str = include_str!("goldens/ollama_ceiling_honoured.sse");
/// The third recording: `ego`'s own body for the Ollama profile, posted
/// unmodified. Response identities and clocks are synthetic in all three recordings.
const ADAPTED_REQUEST: &str = include_str!("goldens/ollama_ceiling_adapted.request.json");
const ADAPTED_TRANSCRIPT: &str = include_str!("goldens/ollama_ceiling_adapted.sse");

/// The ceiling both recorded requests asked for.
const CEILING: u64 = 4;

fn recorded(request: &str) -> serde_json::Value {
    serde_json::from_str(request).expect("the recorded request is valid JSON")
}

/// A recorded response, decoded by **the mapping the Ollama profile names**.
///
/// It would be shorter to drive `openai_completions::StreamDecoder` directly,
/// and it would assert less: the claim `wire::ollama` makes is that Ollama's
/// responses need no decoder of their own, and a test that reaches past the
/// row to the family's decoder is a test that assumes that claim instead of
/// checking it. Going through the row's explicit `Ollama` mapping means
/// replacing the delegation with
/// anything else — an empty stream, a second decoder that drifts — fails here.
///
/// Chunked at seven bytes so the SSE framing is exercised across boundaries
/// rather than handed one whole buffer no real socket would deliver.
async fn replay(transcript: &'static str) -> Vec<AiEvent> {
    let chunks: Vec<Result<bytes::Bytes, HttpError>> = transcript
        .as_bytes()
        .chunks(7)
        .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
        .collect();
    let row = ProviderRow::ollama_default();
    Ollama
        .decode(
            Ok(HttpResponse {
                status: 200,
                headers: vec![
                    HttpHeader::new("content-type", b"text/event-stream".to_vec()).expect("header"),
                ],
                body: Box::pin(futures::stream::iter(chunks)),
            }),
            row.cost.clone(),
        )
        .collect()
        .await
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
#[tokio::test]
async fn ollama_honours_max_tokens_and_ignores_max_completion_tokens() {
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

    let overrun = output_tokens(&replay(IGNORED_TRANSCRIPT).await);
    assert!(
        overrun > CEILING,
        "the daemon was asked for at most {CEILING} output tokens via \
         max_completion_tokens and produced {overrun}; if this now holds, \
         Ollama has started reading the field and the mapping should be \
         revisited"
    );

    let bounded = output_tokens(&replay(HONOURED_TRANSCRIPT).await);
    assert_eq!(
        bounded, CEILING,
        "the daemon was asked for at most {CEILING} output tokens via \
         max_tokens and produced {bounded}"
    );
}

/// The consequence for this repository, stated as a test rather than as a
/// comment: the field the **Ollama profile** sends is the field the daemon
/// reads.
///
/// This test used to assert the opposite, and it was right to: until the row
/// could name a mapping, `ego` sent `max_completion_tokens` and the daemon
/// threw it away. `docs/decisions/`
/// `2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §8 closed that, and
/// this is the same guard pointed at the same seam from the other side.
///
/// It goes through `ProviderRow::ollama_default()` plus its explicit mapping rather than calling
/// `wire::ollama::request_body` directly, because the claim under test is
/// about the *profile*: pointing the row back at the family would then fail
/// here rather than pass against a mapping nothing selects.
///
/// The family half is not decoration. It is the other direction of the same
/// claim — `openai_completions` is unchanged, so no member of the family
/// started receiving a deprecated field because one non-member needed it.
#[test]
fn the_ceiling_the_ollama_profile_sends_is_the_one_this_daemon_reads() {
    let req = GenRequest {
        web_search: None,
        cache: None,
        model: "gemma4:e4b-mlx".to_string(),
        messages: Conversation::new(vec![Message::text(Role::User, "Count from 1 to 40.")])
            .expect("a one-message history is valid"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Low,
        max_output_tokens: Some(CEILING),
        parallel_tool_calls: false,
    };

    let row = ProviderRow::ollama_default();
    let target = ego_ai::wire::RequestTarget::new(row.base(), &[]);
    let request = Ollama
        .build_request(&target, &req)
        .expect("this request is expressible");
    let adapted: serde_json::Value =
        serde_json::from_slice(&request.body.expect("a generation posts a body"))
            .expect("the body is valid JSON");
    assert_eq!(
        adapted["max_tokens"],
        serde_json::json!(CEILING),
        "the Ollama profile stopped sending the field this daemon reads: {adapted}"
    );
    assert!(
        adapted.get("max_completion_tokens").is_none(),
        "one ceiling, one field: a request that asks twice hands a provider \
         that reads them differently a choice nobody made: {adapted}"
    );

    let plain: serde_json::Value = serde_json::from_slice(
        &openai_completions::request_body(&req).expect("this request is expressible"),
    )
    .expect("the body is valid JSON");
    assert_eq!(
        plain["max_completion_tokens"],
        serde_json::json!(CEILING),
        "the family must still get the schema's field, or one non-member's \
         departure has leaked into it: {plain}"
    );
    assert!(plain.get("max_tokens").is_none(), "{plain}");

    // The join, and the reason this file has three recordings rather than two:
    // the body asserted above is byte-for-byte the one that was sent.
    let sent = recorded(ADAPTED_REQUEST);
    assert_eq!(
        adapted, sent,
        "the recording no longer exercises the body this profile builds, so it \
         no longer says anything about our requests"
    );
}

/// The measurement that closes it: `ego`'s own body, against the live daemon.
///
/// The `honoured` recording proves `max_tokens` binds, but it was written by
/// hand to isolate one field. This one was not: the request golden is the
/// exact bytes `ProviderRow::ollama_default()` produces, posted unmodified,
/// and the daemon stopped at the ceiling and said `length`.
///
/// That is the difference between "the field works" and "our request works",
/// and this file exists because the second does not follow from the first —
/// the `reasoning_effort` claim it opens with was true of the field and false
/// of the behaviour.
#[tokio::test]
async fn the_body_the_ollama_profile_builds_is_bounded_by_the_daemon() {
    let events = replay(ADAPTED_TRANSCRIPT).await;
    assert_eq!(
        output_tokens(&events),
        CEILING,
        "the daemon was sent ego's own body asking for at most {CEILING} \
         output tokens: {events:?}"
    );
    assert_eq!(
        stop(&events),
        StopReason::Length,
        "a generation the daemon cut at the ceiling must be Length, not an \
         error and not an ordinary Stop: {events:?}"
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
#[tokio::test]
async fn a_bounded_ollama_generation_is_length_and_not_an_error() {
    let events = replay(HONOURED_TRANSCRIPT).await;
    assert_eq!(stop(&events), StopReason::Length);
    assert!(
        !events.iter().any(|e| matches!(e, AiEvent::Error { .. })),
        "a generation that merely hit its ceiling was reported as an error: {events:?}"
    );
}

/// The unbounded control, from the same daemon and the same prompt: it ends
/// at `stop`, so the `length` above is the ceiling talking and not something
/// this model does on every turn.
#[tokio::test]
async fn the_generation_that_was_not_bounded_ends_at_stop() {
    let events = replay(IGNORED_TRANSCRIPT).await;
    assert_eq!(stop(&events), StopReason::Stop);
}
