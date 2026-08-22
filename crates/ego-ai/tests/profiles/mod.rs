//! The profiles the conformance suite is parameterised over.
//!
//! `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §9:
//! the suite runs **per provider**, not per code path, and every profile gets
//! its own fixtures even where several select the same adapter. That is what
//! makes the fixtures non-redundant rather than copies of one test — a fixture
//! is evidence about a provider's bytes, not about ego's code — and it is why
//! this table exists at all: `tests/conformance.rs` and `tests/smoke.rs` must
//! agree, item for item, about what a profile *is*. One reads the fixtures the
//! other writes.

// Each integration test compiles this module separately, so an item only one
// binary needs is dead code in the other.
#![allow(dead_code)]

use std::path::PathBuf;

use ego_ai::http::HttpRequest;
use ego_ai::provider::{
    ANTHROPIC_BASE, GEMINI_BASE, OLLAMA_BASE, OPENAI_BASE, OPENROUTER_BASE, ProviderRow, XAI_BASE,
};
use ego_core::budget::{Metering, SettlementSource};
use ego_core::generator::types::{
    Conversation, GenRequest, Message, Role, ThinkingLevel, ToolDescriptor,
};

/// Where a fixture came from, stated rather than assumed.
///
/// The two kinds prove different things and the difference is not cosmetic:
/// a **recorded** fixture proves the decoder against a provider's *behaviour*,
/// and a **schema-derived** one proves it against the published *contract*. A
/// suite that mixed them silently would let a hand-assembled guess stand in
/// for evidence about a live server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Captured live, by `tests/smoke.rs` under `EGO_SMOKE_RECORD=1`.
    Recorded,
    /// Hand-assembled from the published specification. Honest about what it
    /// is: a claim about the contract, not about a server.
    SchemaDerived,
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
    /// Where this profile's fixtures came from.
    pub provenance: Provenance,
    /// Where in this provider's error body the sentence it wants read lives.
    ///
    /// A path and not a shared parser, because the envelope is **not** shared:
    /// OpenAI and OpenRouter wrap it as `{"error": {"message": …}}` and xAI
    /// publishes the flatter `{"code": …, "error": "…"}`. Check 3 asks whether
    /// what the provider said survives into the `AiEvent::Error`, and it can
    /// only ask that if it knows which bytes the provider said.
    ///
    /// This is deliberately in the *test* table and not on the row. A feature
    /// bit on a row that a decoder consults is the per-provider branch under a
    /// new name, and §9 forbids it — the mapping keeps one behaviour for both
    /// shapes, relaying a body it could not parse verbatim, and that is exactly
    /// why xAI passes a check written around OpenAI's envelope.
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
    provenance: Provenance::Recorded,
    error_message_at: NESTED,
};

/// The hosted row: schema-derived, and said so.
///
/// This repository holds no key for it, so nothing here has been observed
/// against a live server. The fixtures are assembled from the published
/// specification and prove what the decoder does with that contract — which is
/// worth having and is *not* the acceptance check. `tests/smoke.rs` is where
/// that gap closes, on a machine that has a key and asks for it by name.
pub const OPENAI: Profile = Profile {
    id: "openai",
    thinking: ThinkingLevel::Off,
    base: OPENAI_BASE,
    model: "gpt-5.1",
    row: ProviderRow::openai,
    provenance: Provenance::SchemaDerived,
    error_message_at: NESTED,
};

/// xAI: schema-derived, and a **candidate** rather than a member.
///
/// `2026-08-21-grok-as-a-provider.md`. It is on the family's mapping from its
/// published schema and its row is not in the table
/// `crates/ego-cli/src/model.rs` builds, so `--model xai/…` does not resolve
/// (§9: a candidate is not runnable). These fixtures are what it has instead
/// of a live recording.
///
/// **Its error fixture is the weakest evidence in this directory, and that is
/// stated rather than hidden.** xAI publishes no error-body schema at all — a
/// fact checked against `docs.x.ai`, not assumed — so `error/404.json` is the
/// flat `{"code", "error"}` shape its error-code documentation and its users
/// describe, and not something derived from a specification. It is the first
/// thing a live smoke should overwrite.
pub const XAI: Profile = Profile {
    id: "xai",
    thinking: ThinkingLevel::Off,
    base: XAI_BASE,
    model: "grok-4",
    row: ProviderRow::xai_default,
    provenance: Provenance::SchemaDerived,
    // Flat, and demonstrably not the family's. The mapping relays a body it
    // could not parse verbatim, so the sentence survives anyway — which is the
    // difference between a decode *difference* and a decode *failure*.
    error_message_at: &["error"],
};

/// OpenRouter: schema-derived, and a candidate for a sharper reason.
///
/// The ADR's §7 left an open question about conformance check 1 here that xAI
/// does not raise: whether `usage.cost` needs `usage: {include: true}` in the
/// request body, which the family's projection does not send. OpenRouter's own
/// documentation now answers it — "the `usage: { include: true }` and
/// `stream_options: { include_usage: true }` parameters are deprecated and
/// have no effect. Full usage details are now always included automatically" —
/// so the projection is compatible and, incidentally, so is the
/// `stream_options` it *does* send. That is a reading of a document, which is
/// what `SchemaDerived` means; it is not a recording, so the row stays out of
/// the table.
///
/// The model carries a slash on purpose. `CLI.md` spells `--model` as `P/M`
/// and OpenRouter's ids are themselves `vendor/model`, so this profile is the
/// one that would notice a split on the last separator rather than the first.
pub const OPENROUTER: Profile = Profile {
    id: "openrouter",
    thinking: ThinkingLevel::Off,
    base: OPENROUTER_BASE,
    model: "z-ai/glm-4.7",
    row: ProviderRow::openrouter_default,
    provenance: Provenance::SchemaDerived,
    error_message_at: NESTED,
};

/// Anthropic: schema-derived, a candidate, and the first profile that is not
/// on the family's mapping.
///
/// The other four differ from each other in a base, a resolver, a credential
/// and an error envelope, and agree on every request field. This one shares no
/// request field with them: system is a parameter rather than a role, a tool
/// result is a block rather than a message, the ceiling is `max_tokens` beside
/// a `thinking` object, and the stream has no `[DONE]`. That is what makes it
/// evidence for the ADR's claim rather than a fifth copy of one test — the
/// suite's seven checks are written about *provider behaviour*, so a profile
/// on a second wire format is the first real test of whether they were.
///
/// Its error envelope is the family's nesting by coincidence, not by descent:
/// Anthropic publishes `{"type":"error","error":{"type":…,"message":…}}`, and
/// only the path the check reads is shared.
pub const ANTHROPIC: Profile = Profile {
    id: "anthropic",
    thinking: ThinkingLevel::Off,
    base: ANTHROPIC_BASE,
    model: "claude-opus-5",
    row: ProviderRow::anthropic_default,
    provenance: Provenance::SchemaDerived,
    error_message_at: NESTED,
};

/// Gemini: schema-derived, a candidate, and the third wire format.
///
/// Two mappings could be a special case; three is a seam. This one shares no
/// request field with either of the others — a conversation is a list of
/// *steps* with no role on them, the terminal signal is an interaction
/// `status` rather than any finish reason, and the stream is asked for in the
/// query string.
///
/// It is also the only row that cannot offer every level ego names:
/// `thinking_level` has no `off`, so `ThinkingLevel::Off` is refused at the
/// mapping. That is the cost `docs/14` §3.6 accepted in writing.
pub const GEMINI: Profile = Profile {
    id: "gemini",
    thinking: ThinkingLevel::Minimal,
    base: GEMINI_BASE,
    model: "gemini-3.7-flash",
    row: ProviderRow::gemini_default,
    provenance: Provenance::SchemaDerived,
    error_message_at: NESTED,
};

/// Every profile this build ships a row for.
///
/// Four of the six are candidates: a row exists, so there is something to
/// build a request from and decode an answer with, and it is not in the
/// catalogue a run assembles. That is the shape §9 asks for — the evidence is
/// gathered before the provider is selectable, not after.
pub const ALL: &[&Profile] = &[&OLLAMA, &OPENAI, &XAI, &OPENROUTER, &ANTHROPIC, &GEMINI];

// --- fixtures ---

/// Where this profile's four fixture families live.
pub fn fixture_dir(profile: &Profile) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/goldens/profile")
        .join(profile.id)
}

/// One fixture, read.
pub fn read(profile: &Profile, family: &str, name: &str) -> String {
    let path = fixture_dir(profile).join(family).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "{}: {err}\nRecord it: EGO_SMOKE={} EGO_SMOKE_RECORD=1 \
             cargo test -p ego-ai --test smoke -- --ignored",
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
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        "body": body,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&document).expect("a document assembled here")
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
