//! A row carries a base, a mapping owns its path, and the two are joined in
//! exactly one place.
//!
//! `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §2
//! splits them so that a mapping cannot learn who it serves: what it is handed
//! is a [`RequestTarget`], which has a base, a list of headers it may not
//! interpret, and no provider id at all. The split has a second consequence
//! that is easy to get silently wrong, and this file is mostly about that one.
//!
//! **`join` is not `set_path`.** A base with a prefix —
//! `https://gateway.example/openai/v1/`, which is what an openai-compatible
//! gateway behind a path really looks like — resolves `chat/completions` to
//! `…/openai/v1/chat/completions` under `join` and to
//! `https://gateway.example/chat/completions` under `set_path`. Both are
//! URLs, both parse, and only one reaches the provider. And `join` itself
//! needs the trailing slash: without it the last segment is a *file* and gets
//! replaced, which is the same wrong address by a different route.

use ego_ai::provider::ProviderRow;
use ego_ai::wire::{RequestTarget, ollama, openai_codex_responses, openai_completions};
use url::Url;

fn parse(url: &str) -> Url {
    Url::parse(url).expect("a constant this file controls")
}

/// The default rows address the endpoints their providers serve.
///
/// Written as whole URLs rather than as "base plus path", so the assertion is
/// about the address a request actually goes to and not about the arithmetic
/// that produced it. Codex is here because joining `codex/responses` onto
/// `chatgpt.com/backend-api/` is the whole difference from Platform
/// completions, and `set_path` would send it to the wrong host path.
#[test]
fn the_shipped_rows_address_the_endpoints_their_providers_serve() {
    let openai = ProviderRow::openai();
    assert_eq!(
        RequestTarget::new(openai.base(), &[]).url(openai_completions::PATH),
        parse("https://api.openai.com/v1/chat/completions")
    );

    let ollama = ProviderRow::ollama_default();
    assert_eq!(
        RequestTarget::new(ollama.base(), &[]).url(ollama::PATH),
        parse("http://localhost:11434/v1/chat/completions")
    );

    let codex = ProviderRow::openai_codex_default();
    assert_eq!(
        RequestTarget::new(codex.base(), &[]).url(openai_codex_responses::PATH),
        parse("https://chatgpt.com/backend-api/codex/responses")
    );
}

/// The failure `set_path` would cause, pinned as the behaviour that must not
/// return.
///
/// A gateway serving this wire format under a prefix is not hypothetical —
/// `docs/12-cline-provider-survey.md` lists forty compatible bases — and the
/// wrong answer here is a request sent to a real server at the wrong path,
/// which comes back as a 404 blamed on the provider.
#[test]
fn a_base_with_a_prefix_keeps_it() {
    let row = ProviderRow::openai_at(parse("https://gateway.example/openai/v1/"));
    assert_eq!(
        RequestTarget::new(row.base(), &[]).url(openai_completions::PATH),
        parse("https://gateway.example/openai/v1/chat/completions")
    );
}

/// An operator who omits the trailing slash gets the same address as one who
/// does not.
///
/// The normalisation is in the constructor rather than in the join, because
/// the join is per mapping and the base is per row: a rule enforced once where
/// the value is made cannot be forgotten by the next mapping to be written.
#[test]
fn a_base_without_a_trailing_slash_is_normalised_rather_than_truncating_the_path() {
    let with = ProviderRow::openai_at(parse("https://gateway.example/openai/v1/"));
    let without = ProviderRow::openai_at(parse("https://gateway.example/openai/v1"));
    assert_eq!(without.base(), with.base());
    assert_eq!(
        RequestTarget::new(without.base(), &[]).url(openai_completions::PATH),
        parse("https://gateway.example/openai/v1/chat/completions")
    );
}

/// The row's headers arrive at the request verbatim, after the two the mapping
/// adds about its own body.
///
/// Order matters to the reader and not to HTTP: it is what makes the mapping's
/// contribution legible as the first two entries, and a value the profile sent
/// impossible to mistake for one the mapping invented.
#[test]
fn the_profiles_headers_reach_the_request_unread() {
    let headers = vec![
        ("authorization".to_string(), "Bearer sk-test".to_string()),
        (
            "http-referer".to_string(),
            "https://ego.example".to_string(),
        ),
    ];
    let base = parse("https://api.openai.com/v1/");
    let request = RequestTarget::new(&base, &headers).post_json("chat/completions", b"{}".to_vec());

    assert_eq!(
        request.headers,
        vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "text/event-stream".to_string()),
            ("authorization".to_string(), "Bearer sk-test".to_string()),
            (
                "http-referer".to_string(),
                "https://ego.example".to_string()
            ),
        ]
    );
}
