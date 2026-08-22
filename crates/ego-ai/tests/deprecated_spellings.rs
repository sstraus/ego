//! No mapping sends a spelling the contract it targets has retired.
//!
//! `AGENTS.md` §Milestones carries the rule as prose — *target the current
//! version of every provider contract* — and prose is not a guard. Story
//! 036's fifth criterion asks for the falsifiable form, in the same spirit as
//! `crates/ego-core/tests/near_side.rs`: a check that fails in the pull
//! request that reintroduces a retired field rather than in a review that
//! might not happen.
//!
//! # It reads the bytes, not the source
//!
//! `near_side.rs` greps source because its claim is about what a file may
//! *mention*. This one's claim is about what a provider *receives*, so it
//! serialises each mapping's request body and walks the JSON. A field added
//! through `#[serde(flatten)]`, a rename, or a struct three levels down is
//! caught the same way a top-level field is; a comment naming a dead field is
//! not caught at all, which is right.
//!
//! # A deprecation belongs to a contract, not to a wire shape
//!
//! `max_tokens` is the case that forces this. OpenAI marks it `deprecated:
//! true` in favour of `max_completion_tokens`; Ollama's compatibility surface
//! reads `max_tokens` and discards `max_completion_tokens`, which is the whole
//! reason [`ego_ai::wire::ollama`] exists as a second mapping
//! (`docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
//! §8). One key, retired by one vendor and current for another. So the table
//! below is keyed on **(spelling, mapping)** and never on spelling alone: a
//! mapping is derived from exactly one published contract, and that contract's
//! deprecations are the ones that bind it.
//!
//! # Why a member's own deprecations are not in the table
//!
//! Three rows share the `openai-completions` mapping, and one of them —
//! OpenRouter — documents `stream_options: {include_usage: true}` as
//! "deprecated and have no effect", with full usage returned automatically
//! ([usage-accounting](https://openrouter.ai/docs/use-cases/usage-accounting),
//! read 2026-08-22). That is deliberately **not** a breach here, and the
//! reason is recorded so nobody has to re-derive it:
//!
//! - The field is **current on the contract the mapping targets.** OpenAI's
//!   `ChatCompletionStreamOptions.include_usage` carries no deprecation flag
//!   and is what buys the final usage chunk at all
//!   (`openai/openai-openapi`, `openapi.yaml` 2.3.0, read 2026-08-22). xAI
//!   documents the same requirement; a live Ollama honours it.
//! - OpenRouter retired a **precondition, not a spelling.** Nothing succeeded
//!   it, so there is no second spelling to choose between — which is the shape
//!   `AGENTS.md` actually forbids.
//!
//! Dropping the field to please the one member that ignores it would cost the
//! other three their usage figures and would buy a fifth mapping differing in
//! an inert field. `docs/15-provider-contract-audit.md` §3 is where that
//! argument lives in full.

use ego_ai::wire::{anthropic_messages, gemini_interactions, ollama, openai_completions};
use ego_core::generator::types::{
    ContentBlock, Conversation, GenRequest, Message, Role, ThinkingLevel, ToolDescriptor,
};

/// The four mappings this build serialises a request with, named by module.
///
/// `openai-completions` is what the OpenAI, xAI and OpenRouter rows send;
/// `ollama` is the daemon's own; Anthropic and Gemini each have their own.
/// Two tests keep the list honest in both directions:
/// [`the_table_names_only_mappings_this_build_has`] asserts no entry names a
/// mapping that is gone, and [`every_mapping_this_build_has_is_walked`]
/// asserts no mapping is missing from it.
const OPENAI_COMPLETIONS: &str = "wire::openai_completions";
const OLLAMA: &str = "wire::ollama";
const ANTHROPIC_MESSAGES: &str = "wire::anthropic_messages";
const GEMINI_INTERACTIONS: &str = "wire::gemini_interactions";

/// One spelling a vendor has retired, and the mapping forbidden to send it.
struct Retired {
    /// The JSON key, matched whole and at any depth.
    key: &'static str,
    /// The mapping whose body must not carry it.
    mapping: &'static str,
    /// The vendor's own words, and what replaced the field.
    why: &'static str,
}

/// Every retired spelling this build could plausibly reacquire.
///
/// All four entries are OpenAI's, read from `openai/openai-openapi`'s
/// `openapi.yaml` (version 2.3.0, read 2026-08-22) — the only fields of
/// `CreateChatCompletionRequest` carrying `deprecated: true`. Two of them,
/// `functions` and `function_call`, are the pre-`tools` spelling of tool
/// calling: exactly the "two spellings of one capability" the rule names.
///
/// **Ollama contributes none, and that is a finding rather than an omission.**
/// Its compatibility page documents a supported set and an unsupported set and
/// no deprecated set at all, so there is nothing for this table to say about
/// that mapping. What Ollama *does* have is an unsupported field it discards —
/// `max_completion_tokens` — and that belongs to `tests/ollama_ceiling.rs`,
/// which measured it. Support and deprecation are different claims and mixing
/// them here would make both weaker.
const RETIRED: &[Retired] = &[
    Retired {
        key: "max_tokens",
        mapping: OPENAI_COMPLETIONS,
        why: "\"This value is now deprecated in favor of `max_completion_tokens`, \
              and is not compatible with o-series models\"",
    },
    Retired {
        key: "functions",
        mapping: OPENAI_COMPLETIONS,
        why: "\"Deprecated in favor of `tools`.\"",
    },
    Retired {
        key: "function_call",
        mapping: OPENAI_COMPLETIONS,
        why: "\"Deprecated in favor of `tool_choice`.\"",
    },
    Retired {
        key: "seed",
        mapping: OPENAI_COMPLETIONS,
        why: "marked `deprecated: true` with no replacement named",
    },
];

fn conversation(messages: Vec<Message>) -> Conversation {
    Conversation::new(messages).expect("the histories here are valid")
}

/// A request that reaches every optional field at once.
///
/// A ceiling, a tool, a non-default effort and a completed tool round trip, so
/// that no branch of either serialiser is left unwalked — a retired key that
/// only appears when tools are present would otherwise hide behind a request
/// that has none.
fn widest_request() -> GenRequest {
    GenRequest {
        model: "gpt-5.1".to_string(),
        messages: conversation(vec![
            Message::text(Role::System, "You are terse."),
            Message::text(Role::User, "What is in ./src?"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "Reading it.".to_string(),
                    },
                    ContentBlock::ToolUse {
                        call_id: "call_a".to_string(),
                        name: "read_file".to_string(),
                        args: r#"{"path":"src/main.rs"}"#.to_string(),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: "call_a".to_string(),
                    output: "fn main() {}".to_string(),
                    is_error: false,
                }],
            },
        ]),
        tools: vec![ToolDescriptor {
            name: "read_file".to_string(),
            description: "Read a file from the workspace.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        }],
        thinking: ThinkingLevel::High,
        max_output_tokens: Some(4096),
        parallel_tool_calls: true,
    }
}

/// Each mapping's body for [`widest_request`], as JSON.
///
/// All four, and [`every_mapping_this_build_has_is_walked`] is what keeps the
/// count honest as a fifth lands.
fn bodies() -> Vec<(&'static str, serde_json::Value)> {
    let req = widest_request();
    // Each mapping refuses with a refusal type of its own — the seam has no
    // shared one, deliberately — so each is flattened to its own sentence
    // here rather than unified into a type this test would have to invent.
    [
        (
            OPENAI_COMPLETIONS,
            openai_completions::request_body(&req).map_err(|why| format!("{why:?}")),
        ),
        (
            OLLAMA,
            ollama::request_body(&req).map_err(|why| format!("{why:?}")),
        ),
        (
            ANTHROPIC_MESSAGES,
            anthropic_messages::request_body(&req).map_err(|why| format!("{why:?}")),
        ),
        (
            GEMINI_INTERACTIONS,
            gemini_interactions::request_body(&req).map_err(|why| format!("{why:?}")),
        ),
    ]
    .into_iter()
    .map(|(mapping, built)| {
        let bytes = built
            .unwrap_or_else(|why| panic!("{mapping} cannot express the widest request: {why}"));
        (
            mapping,
            serde_json::from_slice(&bytes).expect("the body is valid JSON"),
        )
    })
    .collect()
}

/// Every object key in `value`, at every depth, with the path that reaches it.
///
/// Paths rather than bare names so a failure says `stream_options.max_tokens`
/// and not `max_tokens`, which is the difference between a message that
/// locates the field and one that starts a search.
fn keys(value: &serde_json::Value, at: &str, found: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let path = if at.is_empty() {
                    key.clone()
                } else {
                    format!("{at}.{key}")
                };
                found.push((key.clone(), path.clone()));
                keys(child, &path, found);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                keys(child, &format!("{at}[{index}]"), found);
            }
        }
        _ => {}
    }
}

/// Every breach of [`RETIRED`] in one body, rendered.
fn breaches(mapping: &str, body: &serde_json::Value) -> Vec<String> {
    let mut found = Vec::new();
    keys(body, "", &mut found);
    found
        .iter()
        .filter_map(|(key, path)| {
            RETIRED
                .iter()
                .find(|retired| retired.mapping == mapping && retired.key == key)
                .map(|retired| format!("{mapping} sends `{path}` — {}", retired.why))
        })
        .collect()
}

/// No mapping's request body carries a spelling its contract retired.
#[test]
fn no_mapping_sends_a_retired_spelling() {
    let mut all = Vec::new();
    for (mapping, body) in bodies() {
        all.extend(breaches(mapping, &body));
    }

    assert!(
        all.is_empty(),
        "a mapping is sending a field the contract it targets has deprecated.\n\
         `AGENTS.md` forbids carrying a second spelling of a capability so that \
         an older model keeps working: the current spelling is the only one this \
         build speaks, and a model that needs the old one is absent from the \
         supplied table instead.\n\n{}",
        all.join("\n")
    );
}

/// The walk can actually find something, and finds it per mapping.
///
/// Without this, [`no_mapping_sends_a_retired_spelling`] is indistinguishable
/// from one whose recursion, key matching or mapping names silently match
/// nothing — and a guard that cannot fail is not a guard. The second half is
/// the part that matters most: the *same* key in the *same* place is a breach
/// for one mapping and correct for the other, which is the claim the table's
/// shape rests on.
#[test]
fn the_walk_finds_a_breach_and_scopes_it_to_the_contract_that_retired_it() {
    let body = serde_json::json!({
        "model": "gpt-5.1",
        "stream_options": { "include_usage": true },
        "max_tokens": 4096,
    });

    let caught = breaches(OPENAI_COMPLETIONS, &body);
    assert_eq!(caught.len(), 1, "expected exactly one breach: {caught:?}");
    assert!(caught[0].contains("max_tokens"), "{}", caught[0]);

    assert!(
        breaches(OLLAMA, &body).is_empty(),
        "`max_tokens` is the only ceiling Ollama reads; it is not retired there"
    );
}

/// A nested key is found, not only a top-level one.
///
/// The body is flat today. It was not always — `stream_options` is an object,
/// `tools[].function` is an object inside an array — and a guard that only
/// reads the first level would pass a retired field moved one level down,
/// which is the likeliest way one comes back.
#[test]
fn the_walk_reaches_a_key_nested_inside_an_array() {
    let body = serde_json::json!({
        "tools": [{ "type": "function", "function": { "functions": [] } }]
    });
    let caught = breaches(OPENAI_COMPLETIONS, &body);
    assert_eq!(caught.len(), 1, "expected exactly one breach: {caught:?}");
    assert!(
        caught[0].contains("tools[0].function.functions"),
        "the failure must locate the field: {}",
        caught[0]
    );
}

/// The modules under `wire/` that are not mappings, and why.
///
/// Named rather than inferred, because "which of these builds a request body"
/// is exactly the question [`every_mapping_this_build_has_is_walked`] exists
/// to stop anyone answering by eye.
const NOT_A_MAPPING: &[&str] = &[
    // A usage decoder. It reads a provider's answer and never writes a
    // request, so there is no body for a retired key to appear in.
    "provider_cost",
    // The SSE framing, shared by the mappings above rather than a mapping of
    // its own. It carries no field of its own into any body.
    "sse",
];

/// Every mapping this build has is walked, not only the ones remembered.
///
/// [`the_table_names_only_mappings_this_build_has`] is the other direction and
/// cannot catch this: it asks that each *table entry* names a real mapping,
/// which stays true no matter how many mappings the table has never heard of.
/// This is the direction that bites. It caught the live case — `bodies()`
/// listed two mappings and the module doc said "there is no third" while
/// `anthropic_messages` and `gemini_interactions` had been serialising
/// requests unwalked, so `AGENTS.md`'s "current models only" was enforced on
/// half the build.
///
/// Read off `wire/mod.rs`'s own declarations, so a fifth mapping is caught by
/// existing, and the two non-mappings must be excluded **by name** in
/// [`NOT_A_MAPPING`] rather than by a pattern that would quietly swallow a
/// third.
#[test]
fn every_mapping_this_build_has_is_walked() {
    let declared = include_str!("../src/wire/mod.rs");
    let walked: Vec<&str> = bodies().iter().map(|(mapping, _)| *mapping).collect();
    for line in declared.lines() {
        let Some(module) = line
            .strip_prefix("pub mod ")
            .and_then(|rest| rest.strip_suffix(';'))
        else {
            continue;
        };
        if NOT_A_MAPPING.contains(&module) {
            continue;
        }
        assert!(
            walked.contains(&format!("wire::{module}").as_str()),
            "`wire::{module}` serialises requests and no body of its is \
             walked, so a spelling its vendor has retired could be sent and \
             nothing here would notice. Add it to `bodies()`, or say why it is \
             not a mapping in `NOT_A_MAPPING`.\nwalked: {walked:?}"
        );
    }
}

/// Every mapping the table names is a mapping this build serialises with.
///
/// A renamed or merged mapping would otherwise leave entries that guard
/// nothing, and the suite would stay green while the rule stopped being
/// enforced — the same failure `near_side.rs` guards against by asserting its
/// file list exists.
#[test]
fn the_table_names_only_mappings_this_build_has() {
    let mappings: Vec<&str> = bodies().iter().map(|(mapping, _)| *mapping).collect();
    for retired in RETIRED {
        assert!(
            mappings.contains(&retired.mapping),
            "`{}` is listed against mapping `{}`, which this build does not \
             serialise with; if the mapping moved, move the entry with it",
            retired.key,
            retired.mapping
        );
    }
}

/// The field the audit turned on is still sent.
///
/// Not a duplicate of `openai_completions.rs`'s `usage_is_always_asked_for`:
/// that one pins the family's body, and this one pins the *conclusion* — that
/// a member's deprecation of an inert precondition did not remove a field the
/// mapping's own contract still requires. It is the assertion that would have
/// failed had regression case resolved the question the other way, and it is here so
/// the module doc above cannot drift away from the code.
///
/// **Two mappings, not all four.** `stream_options.include_usage` is a field
/// of OpenAI's chat-completions schema, which is the contract
/// `openai-completions` targets and the one Ollama's compatibility surface
/// honours. Anthropic and Gemini report usage through their own frames and
/// have no such field to send — asserting it of them would not be a stricter
/// test, it would be a false claim about their contracts. That is the same
/// reason [`RETIRED`] is keyed on (spelling, mapping): a deprecation belongs
/// to a contract, and so does a precondition.
#[test]
fn the_current_usage_precondition_survives_a_members_deprecation_of_it() {
    let family = [OPENAI_COMPLETIONS, OLLAMA];
    let asked: Vec<&str> = bodies()
        .into_iter()
        .filter(|(mapping, _)| family.contains(mapping))
        .map(|(mapping, body)| {
            assert_eq!(
                body["stream_options"],
                serde_json::json!({ "include_usage": true }),
                "{mapping} must still ask for the usage chunk"
            );
            mapping
        })
        .collect();

    // So that scoping the loop cannot become the way this test stops
    // asserting anything: a mapping renamed out of `family` would otherwise
    // leave it filtering everything away and passing.
    assert_eq!(asked, family, "the family this claim is about changed");
}
