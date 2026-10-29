//! The profiles the conformance suite is parameterised over.
//!
//! Each provider profile owns its fixtures even when mappings are shared.
//! `tests/conformance.rs` reads the recordings that
//! `crates/ego-cli/tests/smoke.rs` writes; both must agree on profile identity.

// Each integration test compiles this module separately, so an item only one
// binary needs is dead code in the other.
#![allow(dead_code)]

use std::path::PathBuf;

use ego_ai::http::HttpRequest;
use ego_ai::provider::{
    ANTHROPIC_BASE, Credentials, GEMINI_BASE, GITHUB_COPILOT_BASE, MINIMAX_OAUTH_BASE, NOUS_BASE,
    OLLAMA_BASE, OPENAI_BASE, OPENAI_CODEX_BASE, OPENROUTER_BASE, ProviderRow, XAI_BASE,
};
use ego_ai::wire::anthropic_messages::AnthropicMessages;
use ego_ai::wire::gemini_interactions::GeminiInteractions;
use ego_ai::wire::github_copilot_completions::GithubCopilotCompletions;
use ego_ai::wire::nous_portal::NousPortal;
use ego_ai::wire::ollama::Ollama;
use ego_ai::wire::openai_codex_responses::OpenAiCodexResponses;
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_ai::wire::openai_responses::OpenAiResponses;
use ego_ai::wire::xai_responses::XaiResponses;
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::budget::{Metering, SettlementSource};
use ego_core::generator::types::{
    Conversation, GenRequest, Message, Role, ThinkingLevel, ToolDescriptor,
};
use ego_core::permit::HttpHeader;

/// Where a fixture came from, stated rather than assumed.
///
/// The three kinds prove different things and the difference is not cosmetic:
/// a **recorded** fixture proves the decoder against a provider's *behaviour*,
/// and a **schema-derived** one proves it against the published *contract*. A
/// **captured-client-derived** fixture proves what a pinned client builds or
/// accepts. A suite that mixed them silently would let either offline source
/// stand in for evidence about a live server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Captured live, by `crates/ego-cli/tests/smoke.rs` under
    /// `EGO_SMOKE_RECORD=1`.
    Recorded,
    /// Hand-assembled from the published specification. Honest about what it
    /// is: a claim about the contract, not about a server.
    VendorSchemaDerived,
    /// Derived from a pinned client implementation or captured client bytes,
    /// but never represented as a live recording from the provider itself.
    CapturedClientDerived,
}

/// One provider, and everything both suites need to know about it.
pub struct Profile {
    /// The provider id, which is also the fixture directory's name.
    pub id: &'static str,
    /// The base its row addresses, quoted in a recording's provenance line.
    pub base: &'static str,
    /// The model a live smoke asks for. Not resolved: a smoke is about the
    /// bytes, and resolution is what the product does with them.
    pub model: &'static str,
    /// The row under test, built the way the product builds it.
    pub row: fn() -> ProviderRow,
    /// The explicit mapping used by this offline fixture profile. Dynamic
    /// managed rows are resolved by the provider contract suite; this field
    /// keeps conformance fixtures honest without putting a wire on a row.
    pub wire: &'static dyn WireFormat,
    /// Where this profile's fixtures came from.
    pub provenance: Provenance,
    /// The exact schema, recording, or pinned client revision behind the
    /// profile. Captured-client profiles include a full commit SHA and date.
    pub source: &'static str,
    /// Subscription settlement is flat-plan quota even when the wire reports
    /// cost telemetry, so a smoke must not turn it back into currency.
    pub subscription: bool,
    /// The test table identifies each provider's diagnostic field. Production decoding
    /// recognizes supported error shapes uniformly and retains only diagnostic fields, not
    /// whole bodies or provider-specific feature switches.
    pub error_message_at: &'static [&'static str],
    /// The reasoning level this provider's canonical request asks for.
    ///
    /// Per profile, because **no single level every provider can express
    /// exists**, and that is a finding rather than an inconvenience:
    /// `anthropic-messages` refuses `Minimal` and `gemini-interactions`
    /// refuses `Off`, so a shared constant would have had to pick one of them
    /// to break. It is the same fact `ReasoningEffortSupport` carries in the
    /// product — what a provider can be asked for is the provider's, not the
    /// request's.
    ///
    /// Data in this table and not a branch in either suite, exactly like
    /// [`Self::error_message_at`].
    pub thinking: ThinkingLevel,
}

/// `{"error": {"message": …}}` — the envelope OpenAI defined and most of the
/// family copied.
const NESTED: &[&str] = &["error", "message"];

impl Profile {
    /// How this profile is metered, which is what bounds a smoke's spend.
    pub fn metering(&self) -> Metering {
        if self.subscription {
            return ego_ai::catalogue::subscription("ego/test/subscription-profile/v1");
        }
        match self.provenance {
            // A daemon serving its own weights charges nothing and states
            // nothing. Keyed off the row rather than off the provenance would
            // be better, and the row does not carry it — the metering is
            // resolved beside a row, not inside it
            // (`catalogue.rs`), so the two suites name it here.
            _ if self.id == ego_ai::catalogue::OLLAMA => ego_ai::catalogue::unmetered(),
            _ => Metering::Currency {
                settlement: SettlementSource::Quote,
            },
        }
    }
}

/// The local daemon: recorded, because it can be.
///
/// It costs nothing to call, so its fixtures are captured from a live server
/// and its smoke is the one a developer can run freely.
pub const OLLAMA: Profile = Profile {
    id: "ollama",
    thinking: ThinkingLevel::Off,
    base: OLLAMA_BASE,
    model: "gemma4:12b-mlx",
    row: ProviderRow::ollama_default,
    wire: &Ollama,
    provenance: Provenance::Recorded,
    source: OLLAMA_BASE,
    subscription: false,
    error_message_at: NESTED,
};

/// The hosted row: schema-derived, and said so.
///
/// This repository holds no key for it, so nothing here has been observed
/// against a live server. The fixtures are assembled from the published
/// specification and prove what the decoder does with that contract — which is
/// worth having and is *not* the acceptance check.
/// `crates/ego-cli/tests/smoke.rs` is where that gap closes, on a machine that
/// has a key and asks for it by name.
///
/// The public OpenAI row is selectable. Its fixtures exercise the published
/// Responses contract offline; they do not prove live acceptance.
pub const OPENAI: Profile = Profile {
    id: "openai",
    thinking: ThinkingLevel::Off,
    base: OPENAI_BASE,
    model: "gpt-5.6-sol",
    row: ProviderRow::openai,
    wire: &OpenAiResponses,
    provenance: Provenance::VendorSchemaDerived,
    source: "the OpenAI Responses reference, reasoning guide and streaming guide, read 2026-09-07",
    subscription: false,
    error_message_at: NESTED,
};

/// xAI: schema-derived, and a **candidate** rather than a member.
/// The candidate xAI API-key fixtures are not live acceptance evidence.
/// Its error body has no published schema and is weaker than schema-derived
/// fixtures. OpenRouter reports full usage automatically; its shared usage
/// flag is inert there. The OpenRouter model id deliberately contains a slash
/// to check that `P/M` splits on the first separator, not the last.
pub const OPENROUTER: Profile = Profile {
    id: "openrouter",
    thinking: ThinkingLevel::Off,
    base: OPENROUTER_BASE,
    model: "z-ai/glm-4.7",
    row: ProviderRow::openrouter_default,
    wire: &OpenAiCompletions,
    provenance: Provenance::VendorSchemaDerived,
    source: "the OpenRouter Chat Completions streaming specification",
    subscription: false,
    error_message_at: NESTED,
};

/// Anthropic: schema-derived, selectable, and the first profile that is not on
/// the family's mapping.
/// Anthropic differs in system-message placement, tool-result blocks,
/// `max_tokens`, thinking configuration and stream termination. This profile
/// checks that conformance assertions describe behavior rather than one wire
/// shape. Its nested error envelope shares only the path the check reads.
pub const ANTHROPIC: Profile = Profile {
    id: "anthropic",
    thinking: ThinkingLevel::Off,
    base: ANTHROPIC_BASE,
    model: "claude-opus-5",
    row: ProviderRow::anthropic_default,
    wire: &AnthropicMessages,
    provenance: Provenance::VendorSchemaDerived,
    source: "the Anthropic Messages API streaming specification",
    subscription: false,
    error_message_at: NESTED,
};

/// Gemini's request is a sequence of steps, completion is an interaction status, and streaming
/// uses its body and Accept header. Its thinking mapping refuses `Off` rather than silently
/// enabling reasoning.
pub const GEMINI: Profile = Profile {
    id: "gemini",
    thinking: ThinkingLevel::Minimal,
    base: GEMINI_BASE,
    model: "gemini-3.7-flash",
    row: ProviderRow::gemini_default,
    wire: &GeminiInteractions,
    provenance: Provenance::VendorSchemaDerived,
    source: "the stable Gemini v1 Interactions API specification (read 2026-09-04)",
    subscription: false,
    error_message_at: NESTED,
};

/// Exact pinned client sources for the subscription profiles below.
const SUBSCRIPTION_SOURCE: &str =
    "subscription client contract (recorded 2026-08-24; not live provider evidence)";
const PORTAL_SOURCE: &str =
    "subscription client contract (recorded 2026-08-24; not live provider evidence)";

pub const OPENAI_CODEX: Profile = Profile {
    id: "openai-codex",
    thinking: ThinkingLevel::Medium,
    base: OPENAI_CODEX_BASE,
    model: "gpt-5.6-sol",
    row: ProviderRow::openai_codex_default,
    wire: &OpenAiCodexResponses,
    provenance: Provenance::CapturedClientDerived,
    source: SUBSCRIPTION_SOURCE,
    subscription: true,
    error_message_at: NESTED,
};

pub const GITHUB_COPILOT: Profile = Profile {
    id: "github-copilot",
    thinking: ThinkingLevel::Off,
    base: GITHUB_COPILOT_BASE,
    model: "gpt-4.1",
    row: ProviderRow::github_copilot_default,
    wire: &GithubCopilotCompletions,
    provenance: Provenance::CapturedClientDerived,
    source: SUBSCRIPTION_SOURCE,
    subscription: true,
    error_message_at: NESTED,
};

pub const XAI_OAUTH: Profile = Profile {
    id: "xai-oauth",
    thinking: ThinkingLevel::Low,
    base: XAI_BASE,
    model: "grok-4.6",
    row: ProviderRow::xai_oauth_default,
    wire: &XaiResponses,
    provenance: Provenance::VendorSchemaDerived,
    source: "docs.x.ai Responses, streaming, function-calling, reasoning and cost-tracking (read 2026-08-24)",
    subscription: true,
    error_message_at: NESTED,
};

pub const MINIMAX_OAUTH: Profile = Profile {
    id: "minimax-oauth",
    thinking: ThinkingLevel::Off,
    base: MINIMAX_OAUTH_BASE,
    model: "MiniMax-M2.7",
    row: ProviderRow::minimax_oauth_default,
    wire: &AnthropicMessages,
    provenance: Provenance::CapturedClientDerived,
    source: PORTAL_SOURCE,
    subscription: true,
    error_message_at: NESTED,
};

pub const NOUS: Profile = Profile {
    id: "nous",
    thinking: ThinkingLevel::Off,
    base: NOUS_BASE,
    model: "hermes-3-70b",
    row: ProviderRow::nous_default,
    wire: &NousPortal,
    provenance: Provenance::CapturedClientDerived,
    source: PORTAL_SOURCE,
    subscription: true,
    error_message_at: NESTED,
};

pub const SUBSCRIPTION: &[&Profile] = &[
    &OPENAI_CODEX,
    &GITHUB_COPILOT,
    &XAI_OAUTH,
    &MINIMAX_OAUTH,
    &NOUS,
];

pub const ALL: &[&Profile] = &[
    &OLLAMA,
    &OPENAI,
    &OPENROUTER,
    &ANTHROPIC,
    &GEMINI,
    &OPENAI_CODEX,
    &GITHUB_COPILOT,
    &XAI_OAUTH,
    &MINIMAX_OAUTH,
    &NOUS,
];

// --- fixtures ---

/// Where this profile's four fixture families live.
pub fn fixture_dir(profile: &Profile) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace crates have a parent directory")
        .join("ego-ai/tests/goldens/profile")
        .join(profile.id)
}

/// One fixture, read.
pub fn read(profile: &Profile, family: &str, name: &str) -> String {
    let path = fixture_dir(profile).join(family).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "{}: {err}\nRecord it: EGO_SMOKE={} EGO_SMOKE_RECORD=1 \
             cargo test -p ego-cli --test smoke -- --ignored",
            path.display(),
            profile.id
        )
    })
}

/// Every fixture in one family, by name, sorted.
pub fn family(profile: &Profile, family: &str) -> Vec<(String, String)> {
    let dir = fixture_dir(profile).join(family);
    let mut found: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("{}: {err}", dir.display()))
        .map(|entry| {
            entry
                .expect("a directory entry this process just listed")
                .path()
        })
        .filter(|path| path.is_file())
        .map(|path| {
            let name = path
                .file_name()
                .expect("a file has a name")
                .to_string_lossy()
                .into_owned();
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
            (name, body)
        })
        .collect();
    found.sort();
    found
}

/// Write one fixture, creating its family directory.
///
/// Only ever called under an explicit record opt-in. A test that wrote into
/// the source tree on an ordinary run would make `cargo test` non-idempotent,
/// which is a worse bug than a stale fixture.
pub fn write(profile: &Profile, family: &str, name: &str, contents: &str) {
    let dir = fixture_dir(profile).join(family);
    std::fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    eprintln!("recorded {}", path.display());
}

/// The request fixture: the URL, the method, the header *names*, and the body.
///
/// **Names, never values.** The credential is stripped here, at the point of
/// capture, rather than by a later pass over the file — a redaction that runs
/// afterwards is a redaction that can be forgotten, and the thing it would
/// forget is a live API key in a committed file.
///
/// Shared by both suites because they must agree byte for byte: the smoke
/// writes this document from a request it sent, and check 1 rebuilds the
/// request offline and compares. A second spelling here would let the two
/// disagree about what "the request" means and call it a passing test.
pub fn request_fixture(request: &HttpRequest) -> String {
    let body: serde_json::Value = serde_json::from_slice(
        request
            .body
            .as_deref()
            .expect("every mapping this build ships posts a body"),
    )
    .expect("the body a mapping built is JSON");
    let document = serde_json::json!({
        "url": request.url.as_str(),
        "method": format!("{:?}", request.method),
        "header_names": request
            .headers
            .iter()
            .map(|header| header.name())
            .collect::<Vec<_>>(),
        "body": body,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&document).expect("a document assembled here")
    )
}

// --- the request a row builds ---

/// A credential that is set, whatever the row asks for.
///
/// Conformance check 1 asserts the header *name* arrives and its value does not
/// reach the fixture, so the row has to be resolvable — a row whose variable is
/// unset would produce a request with no credential header at all and the check
/// would pass by having nothing to redact.
pub struct Configured;

impl Credentials for Configured {
    fn secret(
        &self,
        var: &str,
    ) -> Result<ego_ai::provider::Secret, ego_ai::provider::MissingCredential> {
        Ok(match var {
            "oauth:openai-codex" => "head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig".into(),
            "oauth:github-copilot" => "token;proxy-ep=proxy.individual.githubcopilot.com".into(),
            _ => SECRET.into(),
        })
    }
}

/// The value a fixture must never contain.
pub const SECRET: &str = "sk-conformance-secret";

/// The request this profile would send for `req`, with its credential on it.
///
/// Built through the row's own mapping, so what the provider is asked to accept
/// is exactly what the product would send — the whole point of check 5, and the
/// reason it lives beside the table rather than in one suite. The header
/// assembly is [`WireGenerator::build_request`]'s: the credential's headers
/// first, then the row's declared identity headers overlaying them by name. A
/// second spelling of it would let a suite agree with itself about a request the
/// product never sends.
pub fn built(
    row: &ProviderRow,
    wire: &'static dyn WireFormat,
    creds: &dyn Credentials,
    req: &GenRequest,
) -> HttpRequest {
    let credential = row
        .credential
        .resolve(creds)
        .unwrap_or_else(|err| panic!("{} was asked for by name: {err}", row.id()));
    let mut headers = credential.headers();
    for (name, value) in &row.extra_headers {
        headers.retain(|existing| !existing.name().eq_ignore_ascii_case(name));
        headers.push(
            HttpHeader::new(name, value.as_bytes().to_vec())
                .expect("row extra headers are validated public profile headers"),
        );
    }
    let target = RequestTarget::new(credential.base_override().unwrap_or(row.base()), &headers);
    wire.build_request(&target, req)
        .unwrap_or_else(|refused| panic!("{}: {refused}", row.id()))
}

/// One profile's canonical request, built offline against [`Configured`].
pub fn built_canonical(profile: &Profile) -> HttpRequest {
    built(
        &(profile.row)(),
        profile.wire,
        &Configured,
        &canonical(profile),
    )
}

// --- the requests both suites send ---

/// The prompt the canonical request carries. Short, deterministic in length,
/// and dull enough that no provider's safety layer has an opinion about it.
pub const PROMPT: &str = "Reply with the single word: ok";

/// The output ceiling the canonical request carries.
///
/// Sixteen tokens is enough for the word and its framing and nothing else,
/// which is what keeps a live smoke's reservation inside a few cents.
pub const CEILING: u64 = 16;

/// The output ceiling the tool-calling request carries, which has to be
/// larger.
///
/// A tool call is a name and a JSON object, and sixteen tokens does not fit
/// one: the first recording taken under [`CEILING`] came back whole by luck
/// and terminated `length`, one token short of saying `tool_calls`. That is
/// the failure check 2 is *for* — a ceiling that truncates arguments produces
/// a call that looks present and is unparseable — so the request has to leave
/// room for the call rather than the check having to tolerate a cut one.
pub const TOOL_CEILING: u64 = 128;

/// The one request check 1 pins and check 5 sends.
///
/// It lives here rather than in either suite because the two must agree byte
/// for byte: the smoke sends this body and records what came back, and check 1
/// rebuilds it offline and compares against the fixture. Two spellings would
/// let the pair disagree about what was tested and still both pass.
pub fn canonical(profile: &Profile) -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: profile.model.to_string(),
        messages: Conversation::new(vec![Message::text(Role::User, PROMPT)])
            .expect("one user message is a conversation"),
        tools: Vec::new(),
        thinking: profile.thinking,
        max_output_tokens: Some(CEILING),
        parallel_tool_calls: false,
    }
}

/// The same request, offering one tool and asking for it by name.
///
/// The tool is deliberately trivial — one required string — because check 2 is
/// about whether the arguments arrive assembled, not about whether a model can
/// fill in a complicated schema.
pub fn tool_calling(profile: &Profile) -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        messages: Conversation::new(vec![Message::text(
            Role::User,
            "Read the file ./src/main.rs using the read_file tool. \
             Call the tool, do not answer from memory.",
        )])
        .expect("one user message is a conversation"),
        tools: vec![ToolDescriptor {
            name: "read_file".to_string(),
            description: "Read a file from the workspace.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
        }],
        max_output_tokens: Some(TOOL_CEILING),
        ..canonical(profile)
    }
}
