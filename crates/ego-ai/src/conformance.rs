//! Final provider inventory and the cost behavior each profile declares.

use crate::definitions::{DuplicateProvider, ProviderDefinition, provider_rows};
use crate::provider::{CostExpectation, ProviderRow};

/// The final inventory, with the five rows whose base an operator may move.
///
/// `ollama`, `openai` and `anthropic` are parameters because those three are the API-key rows a machine can legitimately serve
/// from somewhere other than the vendor's own hostname, and each publishes the
/// variable that says so: `OLLAMA_HOST`, `OPENAI_BASE_URL`,
/// `ANTHROPIC_BASE_URL`. The subscription rows are bound to one vendor's
/// identity and have no meaning at another address, so they are not
/// parameters. `openrouter` is a
/// parameter because `OPENROUTER_BASE_URL` moves it; it is not one of the
/// API-key rows above, because OpenRouter is itself a gateway.
///
/// `gemini` is here as of
/// the design notes.
/// It was the one implemented row left out of this array, and it was left out
/// for the reason that ADR rejects: no sanitized live recording exists. Its
/// absence was worse than the `openai` refusal it mirrored, because a row that
/// is not in this array has no refusal at all — `-m gemini/…` answered
/// "unknown provider", which is false about a mapping this build ships,
/// fixtures and all.
///
/// `bedrock` is a parameter because its base names the AWS region,
/// which the environment states (`AWS_REGION`). `vertex` is not: its project
/// and location are resolved with its credential.
///
/// Every row goes through [`provider_rows`], so a second definition of one
/// provider id fails here, with both sources named, and does not silently
/// replace the first row in the catalogue.
pub fn final_provider_rows(
    ollama: ProviderRow,
    openai: ProviderRow,
    anthropic: ProviderRow,
    openrouter: ProviderRow,
    bedrock: ProviderRow,
) -> Result<Vec<ProviderRow>, DuplicateProvider> {
    provider_rows([
        ProviderDefinition::new("the `ollama` parameter of the final inventory", ollama),
        ProviderDefinition::new("the `openai` parameter of the final inventory", openai),
        ProviderDefinition::new(
            "ProviderRow::openai_codex_default",
            ProviderRow::openai_codex_default(),
        ),
        ProviderDefinition::new(
            "the `anthropic` parameter of the final inventory",
            anthropic,
        ),
        ProviderDefinition::new(
            "ProviderRow::github_copilot_default",
            ProviderRow::github_copilot_default(),
        ),
        ProviderDefinition::new(
            "ProviderRow::xai_oauth_default",
            ProviderRow::xai_oauth_default(),
        ),
        ProviderDefinition::new(
            "ProviderRow::kimi_coding_default",
            ProviderRow::kimi_coding_default(),
        ),
        ProviderDefinition::new(
            "ProviderRow::minimax_oauth_default",
            ProviderRow::minimax_oauth_default(),
        ),
        ProviderDefinition::new("ProviderRow::nous_default", ProviderRow::nous_default()),
        ProviderDefinition::new(
            "the `openrouter` parameter of the final inventory",
            openrouter,
        ),
        ProviderDefinition::new("ProviderRow::gemini_default", ProviderRow::gemini_default()),
        ProviderDefinition::new("the `bedrock` parameter of the final inventory", bedrock),
        ProviderDefinition::new(
            "ProviderRow::bedrock_runtime_default",
            ProviderRow::bedrock_runtime_default(),
        ),
        ProviderDefinition::new("ProviderRow::vertex", ProviderRow::vertex()),
        ProviderDefinition::new("ProviderRow::vertex_gemini", ProviderRow::vertex_gemini()),
        ProviderDefinition::new(
            "ProviderRow::deepseek_default",
            ProviderRow::deepseek_default(),
        ),
        ProviderDefinition::new("ProviderRow::zai_default", ProviderRow::zai_default()),
        ProviderDefinition::new(
            "ProviderRow::kimi_api_default",
            ProviderRow::kimi_api_default(),
        ),
        ProviderDefinition::new(
            "ProviderRow::mistral_default",
            ProviderRow::mistral_default(),
        ),
    ])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalCostExpectation {
    profile_id: String,
    expected: CostExpectation,
}

impl FinalCostExpectation {
    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    pub fn expected(&self) -> CostExpectation {
        self.expected
    }

    pub fn accepts(&self, observed: CostExpectation) -> bool {
        self.expected == observed
    }
}

pub fn final_cost_expectations() -> Result<Vec<FinalCostExpectation>, DuplicateProvider> {
    Ok(default_final_provider_rows()?
        .iter()
        .map(|row| FinalCostExpectation {
            profile_id: row.id().to_string(),
            expected: row.cost_expectation(),
        })
        .collect())
}

/// The final inventory at every row's default base.
pub fn default_final_provider_rows() -> Result<Vec<ProviderRow>, DuplicateProvider> {
    final_provider_rows(
        ProviderRow::ollama_default(),
        ProviderRow::openai(),
        ProviderRow::anthropic_default(),
        ProviderRow::openrouter_default(),
        ProviderRow::bedrock_default(),
    )
}
