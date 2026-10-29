//! Fetching the litellm table: once, conditionally, and never storing
//! something that is not a table.
//!
//! `docs/guides/providers.md` §6 puts the fetch in
//! this crate, because it owns `Http`, and §7 settles what the run does when it
//! fails. What is tested here is only the exchange — whether the cache is
//! *consulted* is `Metering::ceiling_needs_rates`, one layer up, and whether
//! the network is reachable at all is the transport policy, one layer down.

mod support;

use std::sync::Arc;

use ego_ai::http::Http;
use ego_ai::pricing::{self, FetchError, Fetched};
use support::{Answer, server};

/// A trimmed but genuine document, so what is stored is what would decode.
const TABLE: &str = r#"{
  "gpt-5": {"input_cost_per_token": 1.25e-06, "output_cost_per_token": 1e-05,
            "litellm_provider": "openai", "mode": "chat"}
}"#;

/// A real validator, in GitHub's own spelling — quoted, and 64 hex characters.
///
/// Measured on 2026-08-22: `raw.githubusercontent.com` answers this file with
/// `etag: "8cc8759e…"` and no `last-modified`. It is **not** the sha256 of the
/// bytes — that is `0e8d2323…` for the same fetch — so the two halves of the
/// revision are genuinely two facts and not one written twice.
const ETAG: &str = "\"8cc8759e0212b679048413d9f0c8689706656e46e332f99ceffafecd02b0cff8\"";

fn transport(answers: Vec<Answer>) -> (Arc<dyn Http>, Arc<support::StubServer>) {
    server(answers)
}

/// The ordinary first fetch: bytes, and the identity they were served under.
#[tokio::test]
async fn a_first_fetch_returns_the_bytes_and_the_validator_they_came_with() {
    let (http, stub) = transport(vec![Answer::from((200, TABLE)).with_header("etag", ETAG)]);

    let fetched = pricing::fetch(&http, None).await.expect("a table");
    let Fetched::Table {
        bytes,
        upstream_version,
    } = fetched
    else {
        panic!("a first fetch has nothing to be unchanged against");
    };

    assert_eq!(bytes, TABLE.as_bytes());
    assert_eq!(upstream_version, ETAG);
    assert_eq!(stub.paths(), vec![pricing::upstream().path().to_string()]);
    assert!(
        !stub.asked()[0]
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("if-none-match")),
        "nothing is cached, so there is nothing to be conditional on"
    );
}

/// Knowing a validator makes the request conditional, which is what "fetched
/// once" costs on a refresh: nothing.
#[tokio::test]
async fn a_known_validator_is_offered_back_so_the_server_can_say_nothing_changed() {
    let (http, stub) = transport(vec![Answer::from((304, ""))]);

    let fetched = pricing::fetch(&http, Some(ETAG)).await.expect("a verdict");
    assert!(
        matches!(fetched, Fetched::Unchanged),
        "304 means the cached bytes are still current"
    );
    assert_eq!(
        stub.asked()[0]
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("if-none-match"))
            .map(|(_, value)| value.as_str()),
        Some(ETAG)
    );
}

/// And a changed table answers 200 to the same conditional request.
#[tokio::test]
async fn a_conditional_request_that_is_answered_with_a_body_is_a_new_table() {
    let (http, _) = transport(vec![
        Answer::from((200, TABLE)).with_header("ETag", "\"newer\""),
    ]);

    let Fetched::Table {
        upstream_version, ..
    } = pricing::fetch(&http, Some(ETAG)).await.expect("a table")
    else {
        panic!("a body is a new table");
    };
    // The header name is spelled `ETag` above and matched here regardless:
    // HTTP field names are case-insensitive and nothing normalises them.
    assert_eq!(upstream_version, "\"newer\"");
}

/// Bytes that are not a table never reach the cache.
///
/// The realistic failure is not a corrupted download but an interception: a
/// captive portal, a proxy's rate-limit page, an HTML error served with a 200.
/// §6 stores one file per fetch named by its content hash, so admitting one of
/// these would put a permanent, hash-named non-table in `$EGO_HOME/pricing/`.
#[tokio::test]
async fn a_two_hundred_that_is_not_a_table_is_refused_before_it_can_be_stored() {
    let (http, _) = transport(vec![
        Answer::from((200, "<html>you are behind a proxy</html>")).with_header("etag", ETAG),
    ]);

    let err = pricing::fetch(&http, None)
        .await
        .expect_err("that is not a table");
    assert!(matches!(err, FetchError::NotATable { .. }), "{err}");
}

/// A table that identifies itself with nothing cannot supply a revision.
///
/// §6 requires the `PricingRevision` to name "the source, the upstream version
/// and the content hash". The content hash is ours and the source is a
/// constant; the middle term exists only if the server states one. A
/// `litellm@unknown#<hash>` revision would be the silently-vacuous shape this
/// ADR refuses everywhere else — attributable to bytes, and to no artefact
/// anyone can go and look at.
///
/// It fires only behind something that strips validators, since GitHub always
/// sends one. That operator is told which header is missing, and `models.toml`
/// remains open to them — §5 ranks it above this table anyway.
#[tokio::test]
async fn a_response_with_no_validator_cannot_be_pinned_and_is_refused() {
    let (http, _) = transport(vec![Answer::from((200, TABLE))]);

    let err = pricing::fetch(&http, None)
        .await
        .expect_err("nothing identifies these bytes upstream");
    let FetchError::Unidentified { header } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(header, "etag");
}

/// A non-2xx that is not a 304 is the network failure §7 gives exit 4 to, and
/// it names the status rather than being swallowed into "no table".
#[tokio::test]
async fn a_server_error_is_reported_as_a_failed_fetch() {
    let (http, _) = transport(vec![Answer::from((503, "upstream is down"))]);

    let err = pricing::fetch(&http, None).await.expect_err("503");
    let FetchError::Unreachable { message, .. } = err else {
        panic!("wrong error: {err}");
    };
    assert!(message.contains("503"), "{message}");
}

/// The address is one constant, and it is the raw artefact rather than a page
/// about it.
#[test]
fn the_upstream_is_the_document_itself() {
    let url = pricing::upstream();
    assert_eq!(url.scheme(), "https");
    assert_eq!(url.host_str(), Some("raw.githubusercontent.com"));
    assert!(url.path().ends_with(".json"), "{url}");
}
