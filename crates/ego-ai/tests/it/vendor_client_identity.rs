//! Default requests use ego identity rather than another application's.
//! Operator headers are separate from published OAuth client IDs. These tests
//! check headers; client IDs and protocol fields retain provider-defined values.

use ego_ai::http::{HttpError, HttpResponse};
use ego_ai::oauth::{BuiltInFlow, provider_auth};
use ego_ai::provider::ProviderRow;
use ego_ai::wire::WireFormat;
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_core::generator::port::AiEvent;
use ego_core::permit::HttpHeader;
use futures::StreamExt;

use crate::profiles;
use profiles::{ALL, built_canonical};

/// One vendor-client string a request must not carry, and whose client it is.
struct Impersonation {
    /// Matched case-insensitively against `name: value`, so a marker that is a
    /// header *name* — `x-app` — is caught beside one that is a value.
    marker: &'static str,
    /// The client the marker names, quoted in the failure so the next reader
    /// does not have to recognise a version string.
    client: &'static str,
}

/// The strings the story names, plus the rest of the identity each one came
/// with. Half-removing a disguise is still shipping one: `claude-cli` without
/// `x-app: cli` is the same claim with one field missing, and Copilot's
/// `GitHubCopilotChat` travelled with an editor version and an integration id
/// that say "VS Code" just as loudly.
const FORBIDDEN: &[Impersonation] = &[
    Impersonation {
        marker: "claude-cli",
        client: "Anthropic's Claude Code CLI",
    },
    Impersonation {
        marker: "claude-code-",
        client: "Claude Code's private beta flags",
    },
    Impersonation {
        marker: "x-app",
        client: "Claude Code's application marker",
    },
    Impersonation {
        marker: "githubcopilotchat",
        client: "GitHub's Copilot Chat extension",
    },
    Impersonation {
        marker: "copilot-chat",
        client: "GitHub's Copilot Chat extension",
    },
    Impersonation {
        marker: "vscode",
        client: "Microsoft Visual Studio Code",
    },
];

/// Every breach in one header list, rendered with the header that carries it.
fn breaches(headers: &[HttpHeader]) -> Vec<String> {
    headers
        .iter()
        .flat_map(|header| {
            let rendered = format!(
                "{}: {}",
                header.name(),
                String::from_utf8_lossy(header.value())
            );
            let lowered = rendered.to_ascii_lowercase();
            FORBIDDEN
                .iter()
                .filter(move |forbidden| lowered.contains(forbidden.marker))
                .map(move |forbidden| format!("`{rendered}` names {}", forbidden.client))
        })
        .collect()
}

/// Every subscription credential this build presents, by the provider id
/// [`provider_auth`] dispatches on and a credential of the shape that selects
/// its branch.
///
/// The Anthropic pair is the reason this is a table and not one call: the same
/// provider has two presentations chosen by the credential's *prefix*, and the
/// `sk-ant-oat` one is where the disguise lived.
const CREDENTIALS: &[(&str, &str)] = &[
    ("anthropic", "sk-ant-oat01-secret"),
    ("anthropic", "sk-ant-api03-secret"),
    (
        "github-copilot",
        "tid=1;proxy-ep=proxy.individual.githubcopilot.com",
    ),
    ("xai-oauth", "xai-secret"),
    ("kimi-coding", "kimi-secret"),
    ("minimax-oauth", "minimax-secret"),
    ("nous", "nous-secret"),
    ("openrouter", "sk-or-secret"),
];

/// Every shipped row's own header defaults, scanned at the row rather than
/// through a request.
///
/// The request scan below goes through `profiles::ALL`, which is a list of the
/// profiles that have canonical goldens — not a list of the rows this build
/// ships. `kimi-coding` has a row and no profile, so a header added to it would
/// pass every other test here. That is precisely the drift a guard against
/// impersonation cannot afford: the hole opens by adding a row, which is the
/// same act that would add the header.
#[test]
fn no_shipped_row_carries_another_vendors_client_in_its_own_defaults() {
    use ego_ai::provider::ProviderRow;

    let rows = [
        ProviderRow::ollama_default(),
        ProviderRow::openai_codex_default(),
        ProviderRow::github_copilot_default(),
        ProviderRow::xai_oauth_default(),
        ProviderRow::kimi_coding_default(),
        ProviderRow::minimax_oauth_default(),
        ProviderRow::nous_default(),
        ProviderRow::openrouter_default(),
        ProviderRow::anthropic_default(),
        ProviderRow::gemini_default(),
    ];
    let mut all = Vec::new();
    for row in &rows {
        for (name, value) in &row.extra_headers {
            let rendered = format!("{name}: {value}");
            let lowered = rendered.to_ascii_lowercase();
            for forbidden in FORBIDDEN {
                if lowered.contains(forbidden.marker) {
                    all.push(format!(
                        "{}: `{rendered}` names {}",
                        row.id(),
                        forbidden.client
                    ));
                }
            }
        }
    }
    assert!(
        all.is_empty(),
        "a shipped provider row carries somebody else's client identity in its \
         own defaults.\n\n{}",
        all.join("\n")
    );
}

#[test]
fn no_request_a_default_row_builds_names_another_vendors_client() {
    let mut all = Vec::new();
    for profile in ALL {
        for breach in breaches(&built_canonical(profile).headers) {
            all.push(format!("{}: {breach}", profile.id));
        }
    }
    assert!(
        all.is_empty(),
        "a default provider row is presenting somebody else's client identity.\n\
         ego ships its own identity only; a vendor that gates on its own client \
         is an operator's `[auth.<provider>.headers]` decision, not a built-in \
         (`docs/guides/providers.md`).\n\n{}",
        all.join("\n")
    );
}

#[test]
fn no_credential_presentation_names_another_vendors_client() {
    let mut all = Vec::new();
    for (provider, credential) in CREDENTIALS {
        let headers = provider_auth(provider, credential)
            .unwrap_or_else(|err| panic!("{provider}: {err}"))
            .headers();
        for breach in breaches(&headers) {
            all.push(format!("{provider}: {breach}"));
        }
    }
    assert!(all.is_empty(), "{}", all.join("\n"));
}

/// The login flows too, because `ego auth login` is a request ego sends.
///
/// The Copilot token exchange is the one that carried a disguise, and it is
/// reached by no row at all — it runs before a credential exists — so the row
/// scan above cannot see it.
#[test]
fn no_built_in_login_request_names_another_vendors_client() {
    let mut all = Vec::new();
    for flow in BuiltInFlow::all() {
        let requests = [
            flow.device_authorization_request().ok(),
            flow.refresh_request("refresh-token").ok(),
            flow.copilot_exchange_request("gho_github").ok(),
            flow.minimax_user_code_request("challenge", "state").ok(),
        ];
        for request in requests.into_iter().flatten() {
            for breach in breaches(&request.headers) {
                all.push(format!("{}: {breach}", flow.id()));
            }
        }
    }
    assert!(all.is_empty(), "{}", all.join("\n"));
}

/// The scan can fail, on every entry it names and on the set that was removed.
///
/// Without this, the three tests above are indistinguishable from a scan whose
/// matching, casing or header rendering silently finds nothing — and a guard
/// that cannot fail is not a guard. The loop is the half that keeps [`FORBIDDEN`]
/// honest as it grows: an entry nothing can match is an entry that guards
/// nothing.
#[test]
fn the_scan_catches_every_client_it_names_and_lets_ego_through() {
    for forbidden in FORBIDDEN {
        let probe = vec![
            HttpHeader::new("user-agent", forbidden.marker.as_bytes().to_vec())
                .expect("the probe header name is valid"),
        ];
        assert_eq!(
            breaches(&probe).len(),
            1,
            "the table names `{}` and the scan does not catch exactly it",
            forbidden.marker
        );
    }

    // The exact set `provider_auth` returned for a `sk-ant-oat` credential
    // before this story removed it.
    let removed = vec![
        HttpHeader::new("user-agent", b"claude-cli/2.1.212".to_vec()).unwrap(),
        HttpHeader::new("x-app", b"cli".to_vec()).unwrap(),
        HttpHeader::new(
            "anthropic-beta",
            b"claude-code-20250219,oauth-2025-04-20".to_vec(),
        )
        .unwrap(),
    ];
    let caught = breaches(&removed);
    assert_eq!(caught.len(), 3, "{caught:?}");

    assert!(
        breaches(&[HttpHeader::new(
            "user-agent",
            concat!("ego/", env!("CARGO_PKG_VERSION"))
                .as_bytes()
                .to_vec()
        )
        .unwrap()])
        .is_empty(),
        "ego naming itself is not impersonation"
    );
}

/// The message a mapping reports for one non-200 answer.
async fn refusal(status: u16, body: &str) -> String {
    let chunks: Vec<Result<bytes::Bytes, HttpError>> =
        vec![Ok(bytes::Bytes::from(body.as_bytes().to_vec()))];
    let events: Vec<AiEvent> = OpenAiCompletions
        .decode(
            Ok(HttpResponse {
                status,
                headers: vec![
                    HttpHeader::new("content-type", b"application/json".to_vec()).unwrap(),
                ],
                body: Box::pin(futures::stream::iter(chunks)),
            }),
            None,
        )
        .collect()
        .await;
    events
        .into_iter()
        .find_map(|event| match event {
            AiEvent::Error { message, .. } => Some(message),
            _ => None,
        })
        .expect("a non-200 answer reports an error")
}

/// A vendor that will not accept ego's own identity gets the last word, and
/// the operator gets the recipe.
///
/// Both halves are the criterion: the vendor's sentence survives verbatim —
/// the operator cannot act on "HTTP 403" alone — and the pointer names where
/// the override lives, because ego no longer sends the identity that used to
/// make this refusal impossible to reach.
#[tokio::test]
async fn a_vendor_refusing_egos_identity_is_quoted_and_answered_with_the_recipe() {
    let said = "This credential is supported only by first-party clients.";
    for status in [401, 403] {
        let message = refusal(status, &format!(r#"{{"error":{{"message":"{said}"}}}}"#)).await;
        assert!(message.contains(said), "{status}: {message}");
        assert!(
            message.contains("[auth.<provider>.headers]")
                && message.contains("docs/guides/providers.md"),
            "{status}: {message}"
        );
    }

    // Not every refusal is about identity, and a hint on all of them is noise
    // that teaches an operator to skip the sentence that matters.
    for status in [402, 429] {
        let message = refusal(status, r#"{"error":{"message":"Out of credit."}}"#).await;
        assert!(message.contains("Out of credit."), "{status}: {message}");
        assert!(
            !message.contains("[auth.<provider>.headers]"),
            "{status}: {message}"
        );
    }
}

/// What ego does send where a vendor's wire contract needs an identity.
/// Protocol fields stay even when client impersonation is removed:
/// `originator`, `openai-beta` and the Anthropic OAuth beta header are part
/// of their backend contracts. Identity cleanup must not remove those fields.
#[test]
fn ego_names_itself_where_a_vendor_contract_wants_an_identity() {
    let codex = ProviderRow::openai_codex_default();
    assert!(
        codex
            .extra_headers
            .contains(&("originator".to_string(), "ego".to_string()))
    );
    let agent = codex
        .extra_headers
        .iter()
        .find(|(name, _)| name == "user-agent")
        .map(|(_, value)| value.clone())
        .expect("the Codex row names a client and the name is ego's");
    assert!(agent.starts_with("ego/"), "{agent}");

    let headers = provider_auth("openai-codex", "head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig")
        .expect("the account claim is present")
        .headers();
    assert!(headers.iter().any(|header| {
        header.name() == "openai-beta" && header.value() == b"responses=experimental"
    }));

    let oat = provider_auth("anthropic", "sk-ant-oat01-secret")
        .expect("the oat branch is selected by prefix")
        .headers();
    assert!(
        oat.iter().any(
            |header| header.name() == "anthropic-beta" && header.value() == b"oauth-2025-04-20"
        ),
        "{oat:?}"
    );
}
