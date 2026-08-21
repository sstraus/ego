//! The one check no fixture can make: the provider accepted the body.
//!
//! `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §9
//! splits "the same contract" into five checks. Four are offline and live in
//! `tests/conformance.rs`, which replays committed transcripts. The fifth
//! cannot be made offline **at all**, and that asymmetry is the honest core of
//! the design: a transcript is a recording of a provider that already agreed,
//! so it can prove what the decoder does with a provider's bytes and can never
//! prove that the provider takes ours. Only a live call can, and a live call
//! costs money and needs a network.
//!
//! # Two locks, because either alone is weak
//!
//! - `#[ignore]` on every test — defeated by `cargo test -- --ignored`, which
//!   some CI configurations run by default;
//! - **and** `EGO_SMOKE`, naming providers explicitly:
//!   `EGO_SMOKE=ollama,openai`. A test whose provider is not named returns
//!   early rather than failing, because "you did not ask for this" is not a
//!   defect.
//!
//! The opt-in is a variable that exists for no other purpose. Gating on
//! `OPENAI_API_KEY` would be the accidental-spend case itself: that variable is
//! exported on a developer's machine precisely because `EnvCredentials` reads
//! it, so a key-presence gate would spend money on `cargo test --workspace` for
//! anyone configured to use the product.
//!
//! # What bounds the spend
//!
//! [`BudgetGate`], and nothing invented here. Every smoke generation reserves
//! against a session limit of a few cents with an output ceiling of tens of
//! tokens, a fixed short prompt and thinking off. If the reservation exceeds the limit
//! the gate refuses and the test fails **without sending anything**, which is
//! the fail-closed direction — and it has a second pleasant property: a smoke
//! that could overspend is a budget-gate bug, found by the test that was
//! trying not to spend.
//!
//! # `EGO_SMOKE_RECORD=1` closes the loop
//!
//! With it set, each profile's four fixture families are rewritten from what
//! this run saw, with a provenance header and with the credential header's
//! *value* stripped at the point of capture rather than afterwards. That is
//! where `tests/goldens/profile/<provider>/` comes from, and it is why the
//! offline suite and the live check cannot drift: one writes what the other
//! reads.

use std::sync::Arc;

use ego_ai::http::{EgressPolicy, Http, HttpRequest, HttpResponse, ReqwestHttp};
use ego_ai::provider::{Credentials, EnvCredentials, ProviderRow};
use ego_ai::wire::RequestTarget;
use ego_core::budget::{BudgetGate, Metering, PricingQuote};
use ego_core::cost::{Amount, CurrencyCode, Money, PricingRevision};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{GenRequest, StopReason};
use futures::StreamExt;

mod profiles;
use profiles::{
    OLLAMA, OPENAI, Profile, TOOL_CEILING, canonical, request_fixture, tool_calling, write,
};

/// The variable that turns any of this on, and the only thing it is for.
const OPT_IN: &str = "EGO_SMOKE";
/// The variable that also rewrites the fixtures from what was seen.
const RECORD: &str = "EGO_SMOKE_RECORD";

/// Whether the operator asked for this provider by name.
fn asked_for(provider: &str) -> bool {
    named(&std::env::var(OPT_IN).unwrap_or_default(), provider)
}

/// The gate itself, with the environment lifted out.
///
/// Split from [`asked_for`] so the test below can call *this* rather than
/// re-spell it: a test that asserts its own copy of a predicate passes
/// whatever the real one does, which for a gate that guards paid requests is
/// precisely the wrong thing to be sure of. Setting the variable instead is
/// not open to us — `std::env::set_var` is unsafe in this edition and the
/// workspace denies unsafe — so the pure half is what gets tested and the one
/// remaining unobservable defect is a typo in [`OPT_IN`] itself.
fn named(list: &str, provider: &str) -> bool {
    list.split(',').map(str::trim).any(|item| item == provider)
}

fn recording() -> bool {
    std::env::var(RECORD).as_deref() == Ok("1")
}

fn usd(decimal: &str) -> Money {
    Money::new(
        decimal.parse::<Amount>().expect("a valid amount"),
        CurrencyCode::new("USD").expect("USD is valid"),
    )
}

/// A price list deliberately above every real one, so the reservation the gate
/// admits is an over-estimate of what this run can cost.
///
/// Over-reserving is the safe direction for a bound: the gate refuses too
/// often and never too rarely. A quote copied from a vendor's page would be a
/// number that goes stale in the direction that spends money.
fn pessimistic_quote() -> PricingQuote {
    let rate = |d: &str| Some(d.parse::<Amount>().expect("a valid rate"));
    PricingQuote {
        revision: PricingRevision("smoke-upper-bound".to_string()),
        currency: CurrencyCode::new("USD").expect("USD is valid"),
        uncached_input: rate("0.0001"),
        cache_read: rate("0.0001"),
        cache_write: rate("0.0001"),
        cache_write_extended: None,
        output: rate("0.0005"),
    }
}

/// Capabilities that bound a reservation and claim nothing else.
///
/// A smoke does not resolve a model — resolution is what the *product* does,
/// and this file is about the bytes — so the context window here is only what
/// `BudgetGate::reserve` needs to compute an upper bound.
///
/// The output ceiling is the **largest** any smoke request asks for, not the
/// smallest: `budget::ceilings` takes `requested.min(max)`, so a capability
/// that undercut a request would quietly bound the reservation below what the
/// provider may actually emit — an under-estimate, which is the one direction
/// a spending bound must never err in.
fn caps() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: 1,
        max_context_tokens: Some(8_192),
        max_output_tokens: Some(TOOL_CEILING),
        max_tools: Some(8),
        supports_parallel_tool_calls: false,
        reasoning_carryover: ReasoningCarryover::Unsupported,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
    }
}

/// The request this profile would send for `req`, with its credential on it.
///
/// Built through the row's own mapping, so what the provider is asked to
/// accept is exactly what the product would send — the whole point of check 5.
fn built(row: &ProviderRow, creds: &dyn Credentials, req: &GenRequest) -> HttpRequest {
    let credential = row
        .credential
        .resolve(creds)
        .unwrap_or_else(|err| panic!("{} was asked for by name: {err}", row.id));
    let headers = credential.headers();
    let target = RequestTarget::new(&row.base, &headers);
    row.wire
        .build_request(&target, req)
        .unwrap_or_else(|refused| panic!("{}: {refused}", row.id))
}

/// Prove the spend is bounded *before* anything is sent.
///
/// The reservation is the product's own gate, not a second mechanism: if this
/// request could cost more than a few cents the gate refuses here, and no
/// socket is opened.
fn bounded(req: &GenRequest, metering: &Metering) {
    let quote = pessimistic_quote();
    let quote = match metering {
        Metering::Unmetered { .. } => None,
        _ => Some(quote),
    };
    let mut gate = BudgetGate::new(usd("0.05"));
    gate.reserve(metering, quote.as_ref(), req, &caps())
        .unwrap_or_else(|refused| {
            panic!("a smoke request must fit in five cents, and this one does not: {refused:?}")
        });
}

/// One request, sent for real, with the raw bytes kept.
async fn send(http: &dyn Http, request: HttpRequest) -> (u16, String) {
    let response: HttpResponse = http
        .execute(request)
        .await
        .unwrap_or_else(|err| panic!("the transport failed before the provider answered: {err}"));
    let status = response.status;
    let mut body = Vec::new();
    let mut chunks = response.body;
    while let Some(chunk) = chunks.next().await {
        let bytes = chunk.unwrap_or_else(|err| panic!("the stream was cut: {err}"));
        body.extend_from_slice(&bytes);
    }
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// The events this profile's mapping produces for `transcript`.
async fn replay(row: &ProviderRow, transcript: &str) -> Vec<AiEvent> {
    let response = Ok(HttpResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: Box::pin(futures::stream::once({
            let bytes = bytes::Bytes::copy_from_slice(transcript.as_bytes());
            async move { Ok(bytes) }
        })),
    });
    row.wire.decode(response, row.cost.clone()).collect().await
}

fn transport() -> Arc<dyn Http> {
    Arc::new(
        ReqwestHttp::new(EgressPolicy::Unrestricted)
            .unwrap_or_else(|err| panic!("no transport could be built: {err}")),
    )
}

// --- recording ---

/// A transcript with its provenance stated in the wire's own comment syntax.
///
/// SSE reserves a line beginning `:` for comments and every decoder skips
/// them, so provenance travels *inside* the evidence rather than in a README
/// beside it — which is the gap `docs/12-cline-provider-survey.md:749-760`
/// found in the harness it studied.
fn transcript_fixture(profile: &Profile, model: &str, when: &str, transcript: &str) -> String {
    format!(
        ": recorded from {} against {} on {when}\n\
         : provider {}, ego {}\n{transcript}",
        profile.base,
        model,
        profile.id,
        env!("CARGO_PKG_VERSION"),
    )
}

// --- the checks, per profile ---

/// The whole live check for one profile: accept, decode, and — when asked —
/// record.
async fn smoke(profile: &Profile, when: &str) {
    if !asked_for(profile.id) {
        eprintln!("{} was not named in {OPT_IN}; nothing was sent", profile.id);
        return;
    }
    let row = (profile.row)();
    let creds = EnvCredentials;
    let http = transport();

    // --- check 5, for the canonical body ---
    let req = canonical(profile.model);
    bounded(&req, &profile.metering());
    let request = built(&row, &creds, &req);
    // The fixture is taken from the request before it is sent, because an
    // `HttpRequest` is not `Clone` — and it must not become one: a request
    // carries a credential, and a type that can be duplicated is a credential
    // that can be left in a second place.
    let request_json = request_fixture(&request);
    let (status, transcript) = send(http.as_ref(), request).await;
    assert_eq!(
        status, 200,
        "{} refused the body this build sends: {transcript}",
        profile.id
    );

    // The acceptance claim is narrow on purpose: a 200 proves the server took
    // the request, never that it did what the request said. What is asserted
    // beyond the status is only what the bytes themselves show.
    let events = replay(&row, &transcript).await;
    let stops: Vec<StopReason> = events
        .iter()
        .filter_map(|event| match event {
            AiEvent::Stop(reason) => Some(*reason),
            _ => None,
        })
        .collect();
    assert_eq!(
        stops.len(),
        1,
        "{} produced {} terminal frames: {events:?}",
        profile.id,
        stops.len()
    );
    assert!(
        !events.iter().any(|e| matches!(e, AiEvent::Error { .. })),
        "{} answered 200 and then reported an error: {events:?}",
        profile.id
    );

    if !recording() {
        return;
    }
    write(profile, "request", "canonical.json", &request_json);
    write(
        profile,
        "stream",
        "text.sse",
        &transcript_fixture(profile, profile.model, when, &transcript),
    );

    // --- the tool-calling transcript ---
    let tools = tool_calling(profile.model);
    bounded(&tools, &profile.metering());
    let (status, tool_transcript) = send(http.as_ref(), built(&row, &creds, &tools)).await;
    assert_eq!(status, 200, "{}: {tool_transcript}", profile.id);
    write(
        profile,
        "stream",
        "tool_calls.sse",
        &transcript_fixture(profile, profile.model, when, &tool_transcript),
    );

    // --- the error family ---
    //
    // Provoked by naming a model that cannot exist, which is the one refusal
    // every provider owes an answer to and the cheapest to ask for: it is
    // rejected before any generation happens, so it costs nothing.
    let absent = canonical("this-model-does-not-exist");
    let (status, error_body) = send(http.as_ref(), built(&row, &creds, &absent)).await;
    assert_ne!(
        status, 200,
        "{} answered 200 for a model that does not exist",
        profile.id
    );
    write(
        profile,
        "error",
        &format!("{status}.json"),
        &format!("{}\n", error_body.trim_end()),
    );

    // --- the usage family ---
    //
    // Lifted from the transcripts rather than fetched again: a usage report is
    // part of a stream and there is no way to ask for one on its own. Its own
    // family all the same, because check 4 is about the *shape* of the report
    // — in particular whether the optional breakdown is there — and a file
    // per shape is what makes an absent breakdown visible rather than buried
    // in a transcript.
    for (family, source) in [("text", &transcript), ("tool_calls", &tool_transcript)] {
        for (nth, frame) in usage_frames(source).into_iter().enumerate() {
            write(
                profile,
                "usage",
                &format!("{family}.{nth}.json"),
                &format!("{frame}\n"),
            );
        }
    }
}

/// Every chunk in `transcript` that carries a usage report.
///
/// More than one is legal — the port says so — so this collects rather than
/// finding, and a profile that reports twice gets two files instead of one
/// silently chosen.
fn usage_frames(transcript: &str) -> Vec<String> {
    transcript
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| *line != "[DONE]")
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|chunk| !chunk["usage"].is_null())
        .map(|chunk| serde_json::to_string_pretty(&chunk).expect("a value that was just parsed"))
        .collect()
}

/// The date a recording is stamped with, as `YYYY-MM-DD` in UTC.
///
/// A calendar date rather than a count of seconds, because provenance is read
/// by a person a year later who wants to know how stale the evidence is. The
/// arithmetic is Howard Hinnant's `civil_from_days`, which is exact for every
/// date this program can see and needs no dependency; `days_are_dates` below
/// pins it against dates computed elsewhere.
fn day_to_date(days: i64) -> String {
    // Shift the epoch to 0000-03-01 so a leap day lands at the end of a cycle
    // and February needs no special case.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// Today, in UTC.
///
/// Read in the test rather than inside [`smoke`], so a run that is not
/// recording reads no clock at all.
fn today() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("this machine's clock is after 1970")
        .as_secs();
    day_to_date((seconds / 86_400) as i64)
}

/// The calendar arithmetic, against dates whose answers are known.
///
/// Four cases and each is a different way to be wrong: the epoch itself, a
/// leap day, the day after a leap day, and a century year that *is* a leap
/// year — the rule a naive `% 4` gets right and `% 100` gets wrong.
#[test]
fn days_are_dates() {
    assert_eq!(day_to_date(0), "1970-01-01");
    assert_eq!(day_to_date(59), "1970-03-01");
    assert_eq!(day_to_date(11_016), "2000-02-29");
    assert_eq!(day_to_date(11_017), "2000-03-01");
    assert_eq!(day_to_date(20_686), "2026-08-21");
}

#[tokio::test]
#[ignore = "sends a real request; set EGO_SMOKE=ollama"]
async fn ollama_accepts_the_body_this_build_sends() {
    smoke(&OLLAMA, &today()).await;
}

#[tokio::test]
#[ignore = "sends a real, billable request; set EGO_SMOKE=openai"]
async fn openai_accepts_the_body_this_build_sends() {
    smoke(&OPENAI, &today()).await;
}

/// The gate itself, which is the one thing here that must run offline.
///
/// Without it, an inverted condition would make every smoke silently inert —
/// and an inert paid test looks exactly like a passing one. This asserts
/// [`named`], the predicate [`asked_for`] actually consults, and never sends
/// anything.
#[test]
fn a_provider_that_was_not_named_is_not_smoked() {
    assert!(named("ollama", "ollama"));
    assert!(named("openai, ollama", "ollama"));
    assert!(!named("", "ollama"));
    assert!(!named("openai", "ollama"));
    // A boolean must not enable everything, which is why the variable names
    // providers rather than answering yes.
    assert!(!named("1", "ollama"));
    assert!(!named("true", "openai"));
}
