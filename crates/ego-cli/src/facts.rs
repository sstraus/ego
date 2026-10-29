//! `models.toml`: where a hosted model's price and ceilings are *supplied*.
//!
//! `docs/architecture.md` §1 forbids this
//! repository from writing down a hosted price — "a price written from memory
//! is not a cheaper version of a price, it is a number the budget gate will
//! trust and that nothing can check" — and says the hosted path "works as soon
//! as a quote is configured". This module is the *configuring*. Without it
//! `Catalogue::with_hosted` has no caller and `--model openai/…` can only ever
//! refuse, which would make the refusal a dead end rather than a contract.
//!
//! ## Why its own file, and three layers of it
//!
//! Two locations on disk, `/etc/ego/models.toml` then `$EGO_HOME/models.toml`,
//! with the user's winning — §7's system-then-user order, and pricing is
//! exactly the fleet fact an administrator states once.
//!
//! Below both sits [`BUILTIN`], compiled in. It is there because without it a
//! machine with a valid `ANTHROPIC_API_KEY` and no files could not run a
//! hosted model at all: the selected model answered "no capabilities are
//! configured", which is a true sentence about a product
//! nobody could start. `docs/decisions/2026-09-07-a-builtin-catalogue-layer-
//! below-system-and-user.md` amends the ban on built-in facts to allow it,
//! under three conditions this module enforces: it is the *weakest* layer, it
//! carries no prices, and it states only what a vendor published.
//!
//! **No prices in the builtin layer**, and that is not caution about staleness
//! — it is §1 of `2026-08-21-what-a-model-resolves-to.md` unchanged. A rate
//! this repository wrote down is a number the budget gate would trust and
//! nothing could check. Missing operator rates do not refuse generation;
//! an unknown response cost stays unknown and is not a free one.
//!
//! **The project and project-local layers are not read here, in any posture.**
//! A repository that could state its own model's price could state a low one,
//! and a low rate would undercount real spend against a configured cap. It
//! is a stronger rule than the trust gate, so it is not expressed through it.
//!
//! It is a separate file rather than a `[model."…"]` table inside
//! `config.toml` because `ConfigLayer` is `deny_unknown_fields` over a schema
//! that mirrors the flags, and a price mirrors no flag: it is a fact about the
//! world, not an instruction to this run.
//!
//! ## Why a run with no money ceiling ignores the prices in it
//!
//! `docs/architecture.md` §5 gates every rate
//! source on a ceiling existing: "none of them is consulted at all unless a
//! ceiling is configured". This file is the first of the three, so the gate
//! starts here.
//!
//! It looks like a no-op today, because reading an operator's own file leaks
//! nothing. It is not one. §5's other two sources are a provider pricing
//! endpoint and a fetched third-party table, and §7's first row — "no fetch is
//! attempted, offline or not" — is a *property* of the ceiling gate rather
//! than of the fetch code: an ordinary run contacts nobody but the model
//! provider. A gate that is only added once there is something to leak
//! through has already been wrong once.
//!
//! It is also honest in its own right. A `MeteringSelected` announcing
//! `rates: operator` on a run with no ceiling would claim figures were
//! consulted that nothing consulted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ego_ai::catalogue::{self, HostedModel, ModelSlug, SuppliedAuthority};
use ego_core::budget::PricingQuote;
use ego_core::ceiling::Ceilings;
use ego_core::cost::{Amount, CurrencyCode, PricingRevision};
use ego_core::generator::capabilities::{ModelCapabilities, ReasoningSelectionError};
use serde::Deserialize;

use crate::cli::UsageError;
use crate::config;

/// The file name looked for in both locations.
pub const MODELS_FILE: &str = "models.toml";
/// The administrator's copy.
const SYSTEM_DIR: &str = "/etc/ego";

/// The catalogue compiled into this binary.
///
/// `include_str!` and not a file read at startup: a bootstrap layer that can
/// go missing is not a bootstrap layer, and an installation is one binary.
/// The file's own header explains which providers appear in it and why the
/// rest do not.
const BUILTIN: &str = include_str!("../catalogue/models.toml");

/// The capability rows generated from the pinned models.dev snapshot.
///
/// Written by `scripts/catalogue.py` from
/// `catalogue/policy.toml` and `catalogue/upstream/models.dev.json`; a row here
/// is never edited by hand, and a slug that [`BUILTIN`] also states is a build
/// error ([`builtin`] panics on it).
const BUILTIN_GENERATED: &str = include_str!("../catalogue/models.generated.toml");

/// The date the compiled-in catalogue was last revised from its authorities.
///
/// A date rather than a counter, because what a reader wants to know about a
/// capability claim is *when this release's catalogue was last checked*, and a
/// monotonic integer answers a different question. It is what a retirement
/// refusal names, so a test holds it equal to the `revision =` line in the
/// file itself — a constant that had drifted would send an operator to the
/// wrong catalogue.
pub const BUILTIN_CATALOGUE_REVISION: &str = "2026-10-15";

/// One `models.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Facts {
    /// Ego's explicit startup choice for each provider.
    ///
    /// Kept beside, rather than on, model facts because discovered providers
    /// do not get capability rows from this file. Values remain full slugs so
    /// the same identifier is validated here, persisted, and shown in the
    /// operator's `--model` remedy.
    #[serde(default)]
    default_model: BTreeMap<String, String>,
    /// Per-model execution defaults, separate from supplied capabilities.
    #[serde(default)]
    service_tier: BTreeMap<String, ego_ai::service_tier::ServiceTier>,
    /// Keyed by the full `provider/model` slug, because that is what
    /// `--model` says and what `Catalogue::resolve_hosted` looks up. Keying
    /// by the model half alone would make the same name under two providers
    /// one entry.
    #[serde(default)]
    model: BTreeMap<String, Supplied>,
    /// Slugs removed from the builtin catalogue.
    ///
    /// Decoded from every layer rather than only from [`BUILTIN`], because
    /// the type is one and an operator's file that says `[retired."x"]` is
    /// better answered than rejected. Only the builtin table is *consulted*
    /// (see [`retirement`]) — an operator who wants a slug gone deletes its
    /// row, and one who wants it back states it, which is the layering
    /// already in place rather than a second mechanism beside it.
    #[serde(default)]
    retired: BTreeMap<String, Retired>,
}

/// A tombstone: when the builtin row was removed and what to use instead.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retired {
    /// A replacement slug; it need not be in this catalogue.
    replacement: String,
    /// The date the row was removed or the provider retirement date.
    retired: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Supplied {
    capabilities: ModelCapabilities,
    /// Optional, per `docs/architecture.md` §3.
    /// A run that configures no ceiling needs no prices, so a `[model."x"]`
    /// entry with capabilities alone is a complete entry and resolves.
    /// Capabilities are not optional beside it, and the asymmetry is the
    /// ruling's: operator ruled about prices.
    #[serde(default)]
    pricing: Option<Quote>,
}

/// A price list as an operator writes it.
///
/// Spelled out here rather than deriving `Deserialize` on `PricingQuote`
/// itself: the quote is a value the budget gate consumes, and a type that can
/// be *decoded* from anything a file says is a type that can be decoded from
/// anywhere. This is the one door, and it is in the binary that owns the
/// files.
///
/// Every category is optional and none defaults: `None` is *unpriced* and
/// makes observed usage in that category unpriceable. A missing field must
/// never read as free.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Quote {
    revision: PricingRevision,
    currency: CurrencyCode,
    uncached_input: Option<Amount>,
    cache_read: Option<Amount>,
    cache_write: Option<Amount>,
    cache_write_extended: Option<Amount>,
    output: Option<Amount>,
}

impl From<Quote> for PricingQuote {
    fn from(quote: Quote) -> Self {
        PricingQuote {
            web_search: None,
            revision: quote.revision,
            currency: quote.currency,
            uncached_input: quote.uncached_input,
            cache_read: quote.cache_read,
            cache_write: quote.cache_write,
            cache_write_extended: quote.cache_write_extended,
            output: quote.output,
        }
    }
}

/// Read one file, or nothing if it is not there.
///
/// A file that exists and cannot be parsed is a hard error naming it (§4.5:
/// invalid config is never skipped silently). Falling back to "no facts" would
/// turn a typo in a rate into `HostedModelNotConfigured`, which sends an
/// operator to look for a file they have already written.
fn read(path: &Path) -> Result<Facts, UsageError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text)
            .map_err(|err: toml::de::Error| UsageError(format!("{}: {err}", path.display()))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Facts::default()),
        Err(err) => Err(UsageError(format!(
            "{} could not be read: {err}",
            path.display()
        ))),
    }
}

/// What one entry resolves to: `ego-ai`'s own supplied-facts type, re-exported
/// so callers name it once.
pub type Hosted = HostedModel;

/// [`BUILTIN`], decoded.
///
/// Panics if it does not decode, and that is the right failure: the file is
/// compiled in, so a malformed one is a broken build rather than a broken
/// installation, and a test decodes it so the panic is reached in CI and not
/// on somebody's machine. Returning an empty layer instead would ship a binary
/// whose out-of-the-box path silently is not one.
fn builtin() -> Facts {
    let mut facts: Facts = toml::from_str(BUILTIN)
        .expect("the compiled-in catalogue is valid TOML this build controls");
    let generated: Facts = toml::from_str(BUILTIN_GENERATED)
        .expect("the generated catalogue is valid TOML this build controls");
    facts.model.extend(generated.model);
    facts
}

/// The three layers as one map, the strongest authority winning per key.
///
/// For model facts, whole rows replace rather than merging field by field. For
/// startup defaults, the key is the provider, so one user choice replaces only
/// that provider's weaker choice. Both projections therefore express the same
/// `Builtin < System < User` rule through this function.
fn merge_layers<T>(
    builtin: BTreeMap<String, T>,
    system: BTreeMap<String, T>,
    user: BTreeMap<String, T>,
) -> BTreeMap<String, (SuppliedAuthority, T)> {
    let mut merged = BTreeMap::new();
    for (authority, layer) in [
        (SuppliedAuthority::Builtin, builtin),
        (SuppliedAuthority::System, system),
        (SuppliedAuthority::User, user),
    ] {
        merged.extend(
            layer
                .into_iter()
                .map(|(slug, supplied)| (slug, (authority, supplied))),
        );
    }
    merged
}

/// What the builtin catalogue knows about a slug its provider switched off.
///
/// Only the builtin table is consulted. A tombstone is a claim about a
/// vendor's lineup with a revision attached, and the two file layers have no
/// revision to attach — so a `[retired]` table in an operator's file would
/// produce a refusal naming a catalogue date that says nothing about it.
///
/// The lookup is by full slug and the answer is data, not a message: the
/// refusal is assembled by the caller that also knows which paths an operator
/// may override in.
pub fn retirement(slug: &str) -> Option<ego_ai::catalogue::ResolveError> {
    let mut catalogue = builtin();
    let retired = catalogue.retired.remove(slug)?;
    Some(ego_ai::catalogue::ResolveError::ModelRetired {
        slug: slug.to_owned(),
        replacement: retired.replacement,
        retired: retired.retired,
        overrides: override_paths(),
        revision: BUILTIN_CATALOGUE_REVISION.to_owned(),
    })
}

/// The files that may override the builtin layer, weakest first.
///
/// Built from the same two constants [`supplied`] reads, so a refusal cannot
/// name a location this module does not actually consult. `$EGO_HOME` is
/// resolved rather than printed as the variable, because an operator about to
/// create a file needs the path and not the recipe for it.
fn override_paths() -> Vec<String> {
    let mut paths = vec![
        PathBuf::from(SYSTEM_DIR)
            .join(MODELS_FILE)
            .display()
            .to_string(),
    ];
    if let Some(home) = config::ego_home() {
        paths.push(home.join(MODELS_FILE).display().to_string());
    }
    paths
}

/// Every supplied hosted model, keyed by slug, user's winning over system's.
///
/// `ceilings` decides whether the *prices* come with them. Capabilities always
/// do — a request needs the model's shape whatever it costs — but the rates
/// are §5's first source and are consulted only when a ceiling needs them.
/// The file is read either way, because a malformed `models.toml` is a hard
/// error at any ceiling: an operator who typed a rate wrong should be told so
/// rather than have the typo become invisible on runs that do not read it.
pub fn supplied(ceilings: &Ceilings) -> Result<Vec<(String, HostedModel)>, UsageError> {
    supplied_with_defaults(ceilings).map(|(models, _)| models)
}

/// Resolve per-model execution defaults with the same authority order as facts.
pub(crate) fn service_tier(slug: &str) -> Result<ego_ai::service_tier::ServiceTier, UsageError> {
    let system = read(&PathBuf::from(SYSTEM_DIR).join(MODELS_FILE))?;
    let user = match config::ego_home() {
        Some(home) => read(&home.join(MODELS_FILE))?,
        None => Facts::default(),
    };
    let tier = merge_layers(
        builtin().service_tier,
        system.service_tier,
        user.service_tier,
    )
    .remove(slug)
    .map(|(_, tier)| tier)
    .unwrap_or_default();
    tier.validate(slug).map_err(UsageError)?;
    Ok(tier)
}

type SuppliedCatalogue = (Vec<(String, HostedModel)>, BTreeMap<String, ModelSlug>);

/// Supplied model facts and ego's startup defaults from one coherent read.
///
/// Both projections share the same three decoded layers. Reading them through
/// separate functions would let a concurrent file replacement pair model
/// facts from one document with a default from another.
pub(crate) fn supplied_with_defaults(ceilings: &Ceilings) -> Result<SuppliedCatalogue, UsageError> {
    let builtin = builtin();
    let system = read(&PathBuf::from(SYSTEM_DIR).join(MODELS_FILE))?;
    let user = match config::ego_home() {
        Some(home) => read(&home.join(MODELS_FILE))?,
        None => Facts::default(),
    };
    let models = project(
        merge_layers(builtin.model, system.model, user.model),
        ceilings,
    )?;
    let defaults = project_defaults(merge_layers(
        builtin.default_model,
        system.default_model,
        user.default_model,
    ))?;
    Ok((models, defaults))
}

/// The builtin layer alone, with both file layers explicitly empty.
///
/// `#[cfg(test)]` because that is the whole truth about it: nothing in a
/// running binary wants the weakest layer without the ones above it. What a
/// test wants it for is to assert *where a fact came from* without reading the
/// environment — which [`supplied`] necessarily does, and which would make the
/// answer depend on whether the machine running the suite happens to have a
/// `models.toml`.
#[cfg(test)]
pub(crate) fn builtin_only() -> Vec<(String, HostedModel)> {
    project(
        merge_layers(builtin().model, BTreeMap::new(), BTreeMap::new()),
        &Ceilings::default(),
    )
    .expect("the compiled-in catalogue keys are slugs")
}

/// Merged layers as the catalogue's own type, or the first key that is not a
/// slug.
fn project(
    merged: BTreeMap<String, (SuppliedAuthority, Supplied)>,
    ceilings: &Ceilings,
) -> Result<Vec<(String, HostedModel)>, UsageError> {
    merged
        .into_iter()
        .map(|(slug, (authority, supplied))| {
            ModelSlug::parse(&slug).map_err(|error| UsageError(error.to_string()))?;
            match supplied
                .capabilities
                .resolve_reasoning_effort(supplied.capabilities.default_reasoning_effort)
            {
                Ok(_) | Err(ReasoningSelectionError::DefaultUnstated) => {}
                Err(error) => {
                    return Err(UsageError(format!(
                        "model `{slug}` has invalid capabilities: {error}"
                    )));
                }
            }
            Ok((
                slug,
                HostedModel {
                    capabilities: supplied.capabilities,
                    authority,
                    // A `models.toml` row is billed in money whether or not
                    // the operator wrote its rates down: the provider will
                    // charge either way, and this file is not where that
                    // fact is decided.
                    metering: catalogue::tabulated(),
                    // §5: the first of the three rate sources, and consulted
                    // only when a ceiling needs one. A run with no money
                    // ceiling resolves this row with no rates and runs
                    // unbounded, which is §1's ordinary case.
                    rates: ceilings
                        .needs_rates()
                        .then_some(supplied.pricing)
                        .flatten()
                        .map(|quote| catalogue::operator_rates(quote.into())),
                },
            ))
        })
        .collect()
}

/// Validate the provider-indexed startup policy without changing its slug.
fn project_defaults(
    merged: BTreeMap<String, (SuppliedAuthority, String)>,
) -> Result<BTreeMap<String, ModelSlug>, UsageError> {
    merged
        .into_iter()
        .map(|(provider, (_, value))| {
            let slug = ModelSlug::parse(&value).map_err(|error| UsageError(error.to_string()))?;
            if slug.provider != provider {
                return Err(UsageError(format!(
                    "default model `{value}` is indexed by provider `{provider}`"
                )));
            }
            Ok((provider, slug))
        })
        .collect()
}

#[cfg(test)]
mod critic_tests;

#[cfg(test)]
mod snapshot_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::generator::capabilities::{ImageInputSupport, ReasoningEffortSupport};
    use ego_core::generator::types::ThinkingLevel;

    /// A transport no row can speak must be refused *by the decoder the file
    /// actually goes through*.
    ///
    /// The guard in `ego-core` asserts this against `serde_json`. This file is
    /// TOML, and a guard that proves a property of a decoder the operator
    /// never reaches proves nothing about the operator's file.
    #[test]
    fn a_transport_no_row_can_speak_does_not_decode_from_toml() {
        let entry = |transport: &str| {
            format!(
                r#"
[model."openai/gpt-5.1".capabilities]
schema_version = 6
max_context_tokens = 400000
max_output_tokens = 128000
max_tools = 128
supports_parallel_tool_calls = true
reasoning_carryover = "same_provider_only"
reasoning_efforts = "any"
cancel = "cooperative"
cache = "long"
transport = "{transport}"
"#
            )
        };

        assert!(
            toml::from_str::<Facts>(&entry("sse")).is_ok(),
            "the transport this build speaks must still decode"
        );
        for spelling in ["web_transport", "auto"] {
            assert!(
                toml::from_str::<Facts>(&entry(spelling)).is_err(),
                "`{spelling}` decoded from a TOML file this build reads"
            );
        }
    }

    #[test]
    fn user_layer_wins_same_slug_facts_and_authority_without_filesystem_io() {
        let layer = |context: u64| {
            toml::from_str::<Facts>(&format!(
                r#"
[model."openai/gpt-5.6".capabilities]
schema_version = 6
max_context_tokens = {context}
max_output_tokens = 128000
max_tools = 128
supports_parallel_tool_calls = true
reasoning_carryover = "same_provider_only"
reasoning_efforts = "any"
cancel = "cooperative"
cache = "long"
transport = "sse"
"#
            ))
            .unwrap()
        };
        let builtin = layer(100000);
        let system = layer(200000);
        let user = layer(400000);
        let merged = merge_layers(builtin.model, system.model, user.model);
        let (authority, supplied) = &merged["openai/gpt-5.6"];
        assert_eq!(*authority, ego_ai::catalogue::SuppliedAuthority::User);
        assert_eq!(supplied.capabilities.max_context_tokens, Some(400000));
    }

    #[test]
    fn parsed_default_reasoning_effort_survives_layer_merge() {
        let layer = |context: u64, default: &str| {
            toml::from_str::<Facts>(&format!(
                r#"
[model."openai/gpt-5.6-sol".capabilities]
schema_version = 6
max_context_tokens = {context}
max_output_tokens = 128000
supports_parallel_tool_calls = true
reasoning_carryover = "same_provider_only"
reasoning_efforts = {{ only = ["off", "low", "medium", "high", "xhigh", "max"] }}
default_reasoning_effort = "{default}"
cancel = "cooperative"
cache = "short"
transport = "sse"
"#
            ))
            .unwrap()
            .model
        };

        let merged = merge_layers(
            layer(100000, "low"),
            layer(200000, "medium"),
            layer(400000, "high"),
        );
        let (authority, supplied) = &merged["openai/gpt-5.6-sol"];
        assert_eq!(*authority, SuppliedAuthority::User);
        assert_eq!(
            supplied.capabilities.default_reasoning_effort,
            Some(ThinkingLevel::High)
        );
        assert_eq!(supplied.capabilities.max_context_tokens, Some(400000));
    }

    /// The builtin layer loses to both files, one at a time.
    ///
    /// Separate from the case above because that one proves `User` beats
    /// `System` and would pass with a builtin layer that was never consulted
    /// at all. Each pair here has exactly one file in it, so the builtin row
    /// is the only other candidate and losing is the only way to be absent.
    #[test]
    fn the_builtin_layer_is_the_weakest_of_the_three() {
        let layer = |context: u64| {
            toml::from_str::<Facts>(&format!(
                r#"
[model."openai/gpt-5.6".capabilities]
schema_version = 6
max_context_tokens = {context}
max_output_tokens = 128000
supports_parallel_tool_calls = true
reasoning_carryover = "same_provider_only"
reasoning_efforts = "any"
cancel = "cooperative"
cache = "long"
transport = "sse"
"#
            ))
            .unwrap()
            .model
        };

        for (winner, system, user) in [
            (SuppliedAuthority::System, layer(200000), BTreeMap::new()),
            (SuppliedAuthority::User, BTreeMap::new(), layer(400000)),
        ] {
            let merged = merge_layers(layer(100000), system, user);
            let (authority, supplied) = &merged["openai/gpt-5.6"];
            assert_eq!(*authority, winner, "the builtin row won against a file");
            assert_ne!(
                supplied.capabilities.max_context_tokens,
                Some(100000),
                "the builtin facts survived a file that replaced the row"
            );
        }

        // And with no file at all it is what answers, or the layer is
        // decoration.
        let merged = merge_layers(layer(100000), BTreeMap::new(), BTreeMap::new());
        let (authority, supplied) = &merged["openai/gpt-5.6"];
        assert_eq!(*authority, SuppliedAuthority::Builtin);
        assert_eq!(supplied.capabilities.max_context_tokens, Some(100000));
    }

    /// The compiled-in catalogue decodes, and says what its header says.
    ///
    /// [`builtin`] panics on a malformed file, which is correct for a broken
    /// build and useless if nothing ever calls it before release. This is what
    /// reaches that panic in CI. The revision check is the other half: the
    /// constant is what a retirement refusal prints, so a constant that had
    /// drifted from the file would send an operator to the wrong catalogue,
    /// and nothing else compares the two.
    #[test]
    fn the_compiled_in_catalogue_decodes_and_names_its_own_revision() {
        let catalogue = builtin();
        assert!(
            !catalogue.model.is_empty(),
            "a bootstrap catalogue with no models is not one"
        );
        assert!(
            BUILTIN.contains(&format!("# revision = {BUILTIN_CATALOGUE_REVISION}")),
            "the file's revision line and BUILTIN_CATALOGUE_REVISION disagree"
        );
        for slug in catalogue.model.keys() {
            let slug = ModelSlug::parse(slug).expect("every builtin key is a slug");
            assert!(
                [
                    "openai",
                    "openai-codex",
                    "anthropic",
                    "openrouter",
                    "gemini",
                    "github-copilot",
                    "xai-oauth",
                    "minimax-oauth"
                ]
                .contains(&slug.provider.as_str()),
                "`{slug}` names a provider whose resolver does not read this file",
            );
        }
    }

    /// Every builtin row is priceless, in the literal sense.
    ///
    /// The rule this holds is the one that made the amendment safe:
    /// `2026-08-21-what-a-model-resolves-to.md` §1 forbids this repository
    /// from writing a hosted price down. A `[model."x".pricing]` block added
    /// to the compiled-in file would be a rate the budget gate trusts and
    /// nothing can check, and it would look exactly like the rows beside it.
    #[test]
    fn the_compiled_in_catalogue_states_no_prices() {
        for (slug, supplied) in builtin().model {
            assert!(
                supplied.pricing.is_none(),
                "the builtin catalogue priced `{slug}`",
            );
        }
    }

    #[test]
    fn builtin_startup_defaults_name_current_models() {
        let defaults = builtin().default_model;
        assert_eq!(defaults["openai"], "openai/gpt-6.1-sol");
        assert_eq!(defaults["openai-codex"], "openai-codex/gpt-6.1-sol");
        assert_eq!(defaults["anthropic"], "anthropic/claude-opus-5-5");
    }

    /// gpt-6.1-sol as published on 2026-09-29: the API model page for the
    /// `openai` row, the authenticated Codex models cache for the subscription
    /// row. gpt-6-sol stays selectable beside it.
    #[test]
    fn gpt_6_1_sol_rows_carry_their_published_facts() {
        use ThinkingLevel::{High, Low, Max, Medium, XHigh};
        let rows = builtin().model;
        let efforts = ReasoningEffortSupport::Only([Low, Medium, High, XHigh, Max].into());

        let api = &rows["openai/gpt-6.1-sol"].capabilities;
        assert_eq!(api.max_context_tokens, Some(1_050_000));
        assert_eq!(api.max_output_tokens, Some(128_000));
        assert_eq!(api.reasoning_efforts, efforts);
        assert_eq!(api.default_reasoning_effort, Some(Medium));
        assert!(
            matches!(api.image_input, ImageInputSupport::Supported(_)),
            "the API row reads images"
        );

        let codex = &rows["openai-codex/gpt-6.1-sol"].capabilities;
        assert_eq!(codex.max_context_tokens, Some(272_000));
        assert_eq!(codex.max_output_tokens, Some(128_000));
        assert_eq!(codex.reasoning_efforts, efforts);
        assert_eq!(codex.default_reasoning_effort, Some(Low));

        assert!(rows.contains_key("openai/gpt-6-sol"));
        assert!(rows.contains_key("openai-codex/gpt-6-sol"));
    }

    // Catches: subscription Sol rows lose image support while API rows advertise it.
    #[test]
    fn codex_sol_image_input_is_supported() {
        let rows = builtin().model;
        for model in ["gpt-6-sol", "gpt-6.1-sol"] {
            let caps = &rows[&format!("openai-codex/{model}")].capabilities;
            assert!(
                matches!(caps.image_input, ImageInputSupport::Supported(_)),
                "{model}: {:?}",
                caps.image_input
            );
        }
    }

    #[test]
    fn superseded_builtin_slugs_remain_actionable() {
        for (old, replacement) in [
            ("openai/gpt-5.6-sol", "openai/gpt-6-sol"),
            ("openai-codex/gpt-5.6-sol", "openai-codex/gpt-6-sol"),
            ("openai/gpt-5.6-terra", "openai/gpt-6-luna"),
            ("openai/gpt-5.6-luna", "openai/gpt-6-luna"),
            ("anthropic/claude-opus-5", "anthropic/claude-opus-5-5"),
        ] {
            let refusal = retirement(old)
                .unwrap_or_else(|| panic!("removed builtin slug `{old}` has no retirement"))
                .to_string();
            assert!(refusal.contains(replacement), "{refusal}");
        }
    }

    /// A tombstone refuses by name, and names somewhere to go.
    ///
    /// Read through [`retirement`] rather than the table, because the message
    /// is what an operator sees and the parts that make it actionable — the
    /// replacement, the revision, and a path that exists — are assembled here
    /// and nowhere else.
    #[test]
    fn a_retired_slug_is_refused_with_its_replacement_and_the_revision() {
        let slug = "anthropic/claude-3-opus-20240229";
        let refusal = retirement(slug)
            .expect("that slug is tombstoned")
            .to_string();

        assert!(
            refusal.contains(slug),
            "the refusal must name what was typed"
        );
        assert!(
            refusal.contains("claude-opus-5-5"),
            "the refusal must name the replacement: {refusal}"
        );
        assert!(
            refusal.contains(BUILTIN_CATALOGUE_REVISION),
            "the refusal must name the catalogue that knows it: {refusal}"
        );
        let system_override = PathBuf::from(SYSTEM_DIR)
            .join(MODELS_FILE)
            .display()
            .to_string();
        assert!(
            refusal.contains(&system_override),
            "the refusal must name where to override it: {refusal}"
        );
        assert!(
            retirement("anthropic/claude-opus-5-5").is_none(),
            "a current model must not be reported as retired"
        );
    }
    #[test]
    fn fast_contract_catches_unknown_configured_tier() {
        let facts: Facts = toml::from_str(
            r#"
[service_tier]
"openai/gpt-6.1-sol" = "fast"
"anthropic/claude-opus-5-5" = "standard"
"#,
        )
        .unwrap();
        assert_eq!(
            facts.service_tier["openai/gpt-6.1-sol"],
            ego_ai::service_tier::ServiceTier::Fast
        );
        assert_eq!(
            facts.service_tier["anthropic/claude-opus-5-5"],
            ego_ai::service_tier::ServiceTier::Standard
        );
        assert!(
            toml::from_str::<Facts>(
                r#"[service_tier]
"openai/gpt-6.1-sol" = "turbo"
"#
            )
            .is_err()
        );
    }
}
