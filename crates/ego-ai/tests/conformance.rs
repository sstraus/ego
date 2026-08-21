

use ego_ai::http::{HttpError, HttpResponse};
use ego_ai::provider::{CredentialStrategy, ProviderRow};
use ego_ai::wire::RequestTarget;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::StopReason;
use ego_core::state::Usage;
use futures::StreamExt;

mod profiles;
use profiles::{ALL, Profile, Provenance, canonical, read, request_fixture, write};

/// Rewrites the request fixtures from what the mappings build.
///
/// Check 1 needs no provider — the body is a pure function of the request —
/// so its fixture can be regenerated offline, and a profile with no live
/// server is not stuck without one. The other three families come off a wire
/// and only `tests/smoke.rs` can write them.
const RECORD: &str = "EGO_CONFORMANCE_RECORD";

/// A credential that is set, whatever the row asks for.
///
/// Check 1 asserts the header *name* arrives and its value does not reach the
/// fixture, so the row has to be resolvable — a row whose variable is unset
/// would produce a request with no credential header at all and the check
/// would pass by having nothing to redact.
struct Configured;

impl ego_ai::provider::Credentials for Configured {
    fn bearer(
        &self,
        _var: &str,
    ) -> Result<ego_ai::provider::Secret, ego_ai::provider::MissingCredential> {
        Ok(SECRET.into())
    }
}

/// The value a fixture must never contain.
const SECRET: &str = "sk-conformance-secret";

/// The events this profile's mapping produces for one response.
async fn decode(row: &ProviderRow, status: u16, body: &str, content_type: &str) -> Vec<AiEvent> {
    // Chunked small, so a decoder that assumed a frame arrives whole fails
    // here rather than against a real socket.
    let chunks: Vec<Result<bytes::Bytes, HttpError>> = body
        .as_bytes()
        .chunks(7)
        .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
        .collect();
    row.wire
        .decode(
            Ok(HttpResponse {
                status,
                headers: vec![("content-type".to_string(), content_type.to_string())],
                body: Box::pin(futures::stream::iter(chunks)),
            }),
            row.cost.clone(),
        )
        .collect()
        .await
}

async fn stream_events(profile: &Profile, name: &str) -> Vec<AiEvent> {
    let row = (profile.row)();
    let transcript = read(profile, "stream", name);
    decode(&row, 200, &transcript, "text/event-stream").await
}

fn stops(events: &[AiEvent]) -> Vec<StopReason> {
    events
        .iter()
        .filter_map(|event| match event {
            AiEvent::Stop(reason) => Some(*reason),
            _ => None,
        })
        .collect()
}

fn usages(events: &[AiEvent]) -> Vec<Usage> {
    events
        .iter()
        .filter_map(|event| match event {
            AiEvent::Usage(usage) => Some(*usage),
            _ => None,
        })
        .collect()
}

// --- check 1: request ---

/// The request each profile builds is the one its fixture pins, and it carries
/// no credential into the fixture.
///
/// Compared as JSON rather than as text, so a serialiser that reorders keys is
/// not a failing test — the contract is the document, not the byte order
/// (`tests/openai_completions.rs:20-27` makes the same choice for the same
/// reason).
#[tokio::test]
async fn check_1_the_request_each_profile_builds_is_the_one_it_committed() {
    for profile in ALL {
        let row = (profile.row)();
        let req = canonical(profile.model);
        let credential = row
            .credential
            .resolve(&Configured)
            .expect("Configured answers every variable");
        let headers = credential.headers();
        let target = RequestTarget::new(&row.base, &headers);
        let built = row
            .wire
            .build_request(&target, &req)
            .unwrap_or_else(|refused| panic!("{}: {refused}", profile.id));
        let document = request_fixture(&built);

        if std::env::var(RECORD).as_deref() == Ok("1") {
            write(profile, "request", "canonical.json", &document);
            continue;
        }

        let committed = read(profile, "request", "canonical.json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&document)
                .expect("a document this test assembled"),
            serde_json::from_str::<serde_json::Value>(&committed)
                .expect("a committed fixture is JSON"),
            "{} builds a different request from the one it committed",
            profile.id
        );

        // The header the credential became is named in the fixture, and its
        // value is not. Both halves matter: without the first, a profile that
        // stopped sending its credential would pass; without the second, a
        // recording could carry a live key into the repository.
        if matches!(row.credential, CredentialStrategy::BearerFromEnv { .. }) {
            assert!(
                committed.contains("authorization"),
                "{} authenticates and its request fixture names no credential header",
                profile.id
            );
        }
        assert!(
            !committed.contains(SECRET),
            "{}'s request fixture contains a credential value",
            profile.id
        );
    }
}

// --- check 2: stream ---

/// Every profile's plain transcript terminates exactly once, at the end.
///
/// Exactly once *and last*, because `AttemptStream` keeps only the first
/// terminal frame — so a second `Stop` is invisible downstream and a fixture
/// that produced one would look perfectly healthy while hiding whatever came
/// after it.
#[tokio::test]
async fn check_2_a_plain_transcript_stops_exactly_once_and_last() {
    for profile in ALL {
        let events = stream_events(profile, "text.sse").await;
        assert_eq!(
            stops(&events).len(),
            1,
            "{} does not terminate exactly once: {events:?}",
            profile.id
        );
        assert!(
            matches!(events.last(), Some(AiEvent::Stop(_))),
            "{} put something after its terminal frame: {events:?}",
            profile.id
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::TextDelta { .. })),
            "{}'s plain transcript produced no text at all",
            profile.id
        );
    }
}

/// Every call in a profile's tool transcript arrives whole, and the turn ends
/// saying so.
///
/// The arguments are the part with a real failure mode: providers stream them
/// as JSON fragments, and assembling those is the mapping's job — a decoder
/// that emitted the first fragment would produce a `ToolCall` that looks
/// present and is unparseable.
///
/// **At least one call, not exactly one.** A profile that emits two
/// interleaved calls — fragments of the second arriving between fragments of
/// the first, which is what OpenAI's transcript here holds — is harder
/// evidence for the same claim, and a count of one would reject it for being
/// too demanding a test.
///
/// Ending as `ToolUse` is what proves the call was not *truncated*. A ceiling
/// that cut the arguments mid-object terminates `Length` and leaves arguments
/// that may still happen to parse, so the stop reason is the only thing that
/// separates "the provider finished asking" from "we stopped it mid-sentence".
#[tokio::test]
async fn check_2_a_tool_transcript_delivers_whole_calls() {
    for profile in ALL {
        let events = stream_events(profile, "tool_calls.sse").await;
        let calls: Vec<(&str, &str, &str)> = events
            .iter()
            .filter_map(|event| match event {
                AiEvent::ToolCall {
                    call_id,
                    name,
                    args,
                } => Some((call_id.as_str(), name.as_str(), args.as_str())),
                _ => None,
            })
            .collect();
        assert!(
            !calls.is_empty(),
            "{}'s tool transcript produced no call at all: {events:?}",
            profile.id
        );
        for (call_id, name, args) in &calls {
            assert!(!call_id.is_empty(), "{}: a call with no id", profile.id);
            assert!(!name.is_empty(), "{}: a call with no name", profile.id);
            serde_json::from_str::<serde_json::Value>(args).unwrap_or_else(|err| {
                panic!(
                    "{}: the assembled arguments of {name} are not JSON ({err}): {args}",
                    profile.id
                )
            });
        }
        assert_eq!(
            stops(&events),
            vec![StopReason::ToolUse],
            "{} did not end its tool-calling turn as a tool call",
            profile.id
        );
    }
}

// --- check 3: error ---

/// A non-200 becomes the three events `wire::failed` exists to keep to one
/// spelling, carrying what the provider said.
///
/// `PossiblySent` and not `DefinitelyUnsent`: a status line at all proves the
/// request arrived, and reading a refusal as "nothing was sent" would licence
/// a retry of a request the provider already has.
#[tokio::test]
async fn check_3_a_refusal_is_three_events_and_repeats_the_provider() {
    for profile in ALL {
        let row = (profile.row)();
        for (name, body) in profiles::family(profile, "error") {
            let status: u16 = name
                .trim_end_matches(".json")
                .parse()
                .unwrap_or_else(|err| panic!("{}: {name} is not a status ({err})", profile.id));
            let events = decode(&row, status, &body, "application/json").await;

            assert_eq!(
                events.first(),
                Some(&AiEvent::DeliveryObserved(DeliveryCertainty::PossiblySent)),
                "{} did not report the request as sent for {name}: {events:?}",
                profile.id
            );
            let message = events
                .iter()
                .find_map(|event| match event {
                    AiEvent::Error { message, .. } => Some(message.as_str()),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    panic!("{} reported no error for {name}: {events:?}", profile.id)
                });
            // What the provider said, not a sentence this build invented. The
            // fixture's own prose is the only thing that can prove it was
            // carried rather than replaced.
            let said = provider_message(profile, &body);
            assert!(
                message.contains(&said),
                "{} dropped what the provider said for {name}: {message:?} does not contain {said:?}",
                profile.id
            );
            assert_eq!(
                stops(&events),
                vec![StopReason::Error],
                "{} did not end {name} as an error",
                profile.id
            );
        }
    }
}

/// The sentence inside a provider's error envelope, wherever that provider
/// keeps it.
///
/// **They do not all keep it in the same place**, and an earlier version of
/// this helper assumed they did. OpenAI, Ollama and OpenRouter wrap it as
/// `{"error": {"message": …}}`; xAI publishes no error-body schema at all and
/// answers with a flatter `{"code": …, "error": "…"}`. Hard-coding the nested
/// path would have made this check silently unfalsifiable for xAI — it would
/// panic on the fixture, or worse, be quietly relaxed until it passed.
///
/// That the mapping needs no such knowledge is the actual finding. It parses
/// the family's envelope and **relays a body it could not parse verbatim**, so
/// xAI's sentence reaches the `AiEvent::Error` inside the raw JSON and check 3
/// passes. A decode *difference* that the fallback already covers is not a
/// decode *failure*, and only a failure would cost xAI its membership.
fn provider_message(profile: &Profile, body: &str) -> String {
    let document: serde_json::Value = serde_json::from_str(body).expect("an error fixture is JSON");
    let mut at = &document;
    for key in profile.error_message_at {
        at = &at[key];
    }
    at.as_str()
        .unwrap_or_else(|| {
            panic!(
                "{}'s error fixture carries no message at {:?}: {body}",
                profile.id, profile.error_message_at
            )
        })
        .to_string()
}

// --- check 4: usage ---

/// Each profile reports its usage exactly once, and the parts partition the
/// total rather than double-counting the cache.
///
/// This is the non-obvious one, and two codebases derived it independently:
/// the wires disagree about whether the prompt figure *includes* the cached
/// tokens. `Usage`'s four input fields are disjoint, so a mapping over an
/// inclusive wire must subtract and one over an exclusive wire must not — and
/// getting it backwards is a silent factor-of-two in the bill, in whichever
/// direction nobody checked.
#[tokio::test]
async fn check_4_usage_is_reported_once_and_partitions_the_total() {
    for profile in ALL {
        for name in ["text.sse", "tool_calls.sse"] {
            let events = stream_events(profile, name).await;
            let reported = usages(&events);
            let usage = match reported.as_slice() {
                [one] => one,
                other => panic!(
                    "{} reported usage {} times for {name}: {other:?}",
                    profile.id,
                    other.len()
                ),
            };
            let total = read(profile, "usage", &usage_fixture_name(name));
            let stated: serde_json::Value =
                serde_json::from_str(&total).expect("a usage fixture is JSON");
            let stated = &stated["usage"];

            let prompt = stated["prompt_tokens"]
                .as_u64()
                .unwrap_or_else(|| panic!("{}: {name} states no prompt total", profile.id));
            let completion = stated["completion_tokens"]
                .as_u64()
                .unwrap_or_else(|| panic!("{}: {name} states no completion total", profile.id));

            assert_eq!(
                usage.uncached_input_tokens
                    + usage.cache_read_tokens
                    + usage.cache_write_tokens
                    + usage.cache_write_extended_tokens,
                prompt,
                "{}'s input parts do not add up to the {name} prompt total",
                profile.id
            );
            assert_eq!(
                usage.output_tokens, completion,
                "{} reported a different output total for {name}",
                profile.id
            );
        }
    }
}

/// The usage fixture belonging to one transcript.
///
/// `text.sse` → `text.0.json`, which is `tests/smoke.rs`'s naming: the family
/// is indexed because more than one report per stream is legal.
fn usage_fixture_name(transcript: &str) -> String {
    format!("{}.0.json", transcript.trim_end_matches(".sse"))
}

/// A row that says the provider states a charge is a row whose transcripts
/// state one — and a row that says it does not is a row whose transcripts do
/// not.
///
/// Both halves, because `cost: None` is not "we have not got round to it": it
/// is the row asserting that no cost will arrive, which is the assertion that
/// makes `UnknownCost::ProviderReportedCostMissing` mean anything on the rows
/// that carry a decoder. An unchecked `None` would let a provider quietly
/// start reporting a figure that nothing read.
///
/// This is the fifth thing a fixture can be evidence about, and it belongs
/// beside check 4 rather than in `provider_cost.rs`: that file proves the
/// decoders against handcrafted `usage` objects, and this proves the *row's
/// own claim* against the same bytes every other check reads.
#[tokio::test]
async fn a_rows_cost_claim_is_what_its_transcripts_do() {
    for profile in ALL {
        let row = (profile.row)();
        for name in ["text.sse", "tool_calls.sse"] {
            let events = stream_events(profile, name).await;
            let reported: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    AiEvent::ProviderCost { cost } => Some(cost),
                    _ => None,
                })
                .collect();
            match &row.cost {
                Some(decoder) => assert_eq!(
                    reported.len(),
                    1,
                    "{} carries a decoder for {} and its {name} reported {} costs",
                    profile.id,
                    decoder.revision().field,
                    reported.len()
                ),
                None => assert!(
                    reported.is_empty(),
                    "{} says it states no charge and its {name} states {reported:?}",
                    profile.id
                ),
            }
        }
    }
}

// --- the suite's own guard ---

/// Every profile has all four families, with something in each.
///
/// The four checks above iterate over what they find, so a family that was
/// never recorded makes them assert nothing and pass. This is the test that
/// notices — the same reason `tests/near_side.rs` checks that the files it
/// greps exist.
#[test]
fn every_profile_has_all_four_families() {
    for profile in ALL {
        for family in ["request", "stream", "error", "usage"] {
            let found = profiles::family(profile, family);
            assert!(
                !found.is_empty(),
                "{} has no {family} fixtures; membership is not partial",
                profile.id
            );
        }
        // Named rather than counted, because check 2 asks two different
        // questions of two different transcripts and a profile with two copies
        // of one of them would satisfy a count.
        let streams: Vec<String> = profiles::family(profile, "stream")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(
            streams.contains(&"text.sse".to_string())
                && streams.contains(&"tool_calls.sse".to_string()),
            "{} is missing a stream fixture: {streams:?}",
            profile.id
        );
    }
}

/// Every transcript states where it came from, and states the same thing its
/// profile does.
///
/// The distinction the suite rests on is `Recorded` versus `SchemaDerived`,
/// and until here it lived only in the table: a hand-assembled transcript
/// dropped into a recorded profile's directory would be read as evidence about
/// a live server by every check above, silently. SSE reserves a line beginning
/// `:` for comments, so the claim travels inside the evidence and moves with
/// the file — which a README beside it would not.
///
/// Only the stream family, because only SSE has a comment syntax. JSON has
/// none, so a request, error or usage fixture's provenance is the profile's
/// and there is nowhere in the file to repeat it.
#[test]
fn every_transcript_says_where_it_came_from() {
    for profile in ALL {
        let expected = match profile.provenance {
            Provenance::Recorded => ": recorded from ",
            Provenance::SchemaDerived => ": schema-derived from ",
        };
        for (name, body) in profiles::family(profile, "stream") {
            let first = body.lines().next().unwrap_or_default();
            assert!(
                first.starts_with(expected),
                "{}/{name} is declared {:?} and opens {first:?}; a transcript must \
                 state its provenance in its first line",
                profile.id,
                profile.provenance
            );
        }
    }
}
