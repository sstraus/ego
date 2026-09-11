//! contract for operator headers on the login exchange (regression case).
//!
//! Removing the vendor disguise left one request with no way back: the Copilot
//! token exchange talks to an endpoint that may gate on a client it
//! recognises, and `[auth.<provider>.headers]` reached only generation
//! requests. An operator who needs that endpoint had no escape hatch at all.
//!
//! What is proved here is that the same declared table now reaches every login
//! request, and that reaching them buys no new authority: the header names
//! that carry credentials stay refused even if a caller hands them in, because
//! the login exchange is exactly where a mistaken override would leak one.

use std::collections::HashMap;

use ego_ai::http::HttpRequest;
use ego_ai::oauth::{LoginHeaders, built_in};

fn value<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a [u8]> {
    request
        .headers
        .iter()
        .find(|header| header.name().eq_ignore_ascii_case(name))
        .filter(|header| !header.is_secret())
        .map(|header| header.value())
}

fn count(request: &HttpRequest, name: &str) -> usize {
    request
        .headers
        .iter()
        .filter(|header| header.name().eq_ignore_ascii_case(name))
        .count()
}

fn declared(pairs: &[(&str, &str)]) -> LoginHeaders {
    LoginHeaders::new(
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<HashMap<_, _>>(),
    )
}

/// The whole point of the escape hatch: what an operator declares arrives on
/// the exchange that used to carry the disguise.
#[test]
fn a_declared_header_reaches_the_copilot_token_exchange() {
    let flow = built_in("github-copilot").expect("the built-in Copilot flow");
    let request = flow
        .copilot_exchange_request("gho_github")
        .expect("the exchange builds");
    let request = declared(&[("editor-version", "ego/0.1.0")]).apply(request);
    assert_eq!(
        value(&request, "editor-version"),
        Some(b"ego/0.1.0".as_ref())
    );
}

/// Ego still says who it is. An override adds an operator's claim; it does not
/// replace ego's own name with one.
#[test]
fn ego_still_names_itself_when_an_operator_adds_headers() {
    let flow = built_in("github-copilot").expect("the built-in Copilot flow");
    let request = flow
        .copilot_exchange_request("gho_github")
        .expect("the exchange builds");
    let request = declared(&[("editor-version", "ego/0.1.0")]).apply(request);
    let agent = value(&request, "user-agent").expect("ego names itself");
    assert!(
        agent.starts_with(b"ego/"),
        "ego must still be the user agent: {:?}",
        String::from_utf8_lossy(agent)
    );
}

/// Defence in depth. Config already refuses these names at deserialization,
/// so a caller reaching this far with one is a bug, not an operator choice —
/// and the login exchange is the single request where honouring it would hand
/// a declared table the bearer token itself.
#[test]
fn a_credential_header_is_refused_even_when_handed_in_directly() {
    let flow = built_in("github-copilot").expect("the built-in Copilot flow");
    let original = flow
        .copilot_exchange_request("gho_github")
        .expect("the exchange builds");
    assert_eq!(count(&original, "authorization"), 1);
    let request = declared(&[
        ("authorization", "Bearer stolen"),
        ("x-api-key", "stolen"),
        ("editor-version", "ego/0.1.0"),
    ])
    .apply(original);
    assert_eq!(
        count(&request, "authorization"),
        1,
        "an override must neither replace nor duplicate the credential"
    );
    assert!(
        request
            .headers
            .iter()
            .find(|header| header.name() == "authorization")
            .expect("the exchange still bears a credential")
            .is_secret(),
        "the bearer token must still be secret material, not a readable override"
    );
    assert_eq!(count(&request, "x-api-key"), 0);
    assert_eq!(
        value(&request, "editor-version"),
        Some(b"ego/0.1.0".as_ref())
    );
}

/// An empty table is the default, and the default is exactly the request the
/// story shipped: ego's identity and nothing else.
#[test]
fn an_empty_table_leaves_the_request_byte_identical() {
    let flow = built_in("github-copilot").expect("the built-in Copilot flow");
    let before = flow
        .copilot_exchange_request("gho_github")
        .expect("the exchange builds");
    let names: Vec<_> = before
        .headers
        .iter()
        .map(|header| header.name().to_owned())
        .collect();
    let after = LoginHeaders::default().apply(
        flow.copilot_exchange_request("gho_github")
            .expect("the exchange builds"),
    );
    let after_names: Vec<_> = after
        .headers
        .iter()
        .map(|header| header.name().to_owned())
        .collect();
    assert_eq!(names, after_names);
    assert_eq!(value(&after, "user-agent"), value(&before, "user-agent"));
}

/// Every built-in login request goes through the same table, not just the one
/// that lost its disguise. A per-request escape hatch would be a trap: the
/// next vendor to gate a login would need another one.
#[test]
fn the_same_table_reaches_a_device_authorization_request() {
    let flow = built_in("xai-oauth").expect("the built-in xAI flow");
    let request = flow
        .device_authorization_request()
        .expect("the device request builds");
    let request = declared(&[("x-operator", "declared")]).apply(request);
    assert_eq!(value(&request, "x-operator"), Some(b"declared".as_ref()));
}
