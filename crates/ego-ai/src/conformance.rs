//! Final provider inventory and the cost behavior each profile declares.

use crate::provider::{CostExpectation, ProviderRow};

pub const FINAL_PROFILE_COUNT: usize = 11;

/// The final inventory, with the three rows whose base an operator may move.
///
/// `ollama`, `openai` and `anthropic` are parameters and the rest are not,
/// because those three are the API-key rows a machine can legitimately serve
/// from somewhere other than the vendor's own hostname, and each publishes the
/// variable that says so: `OLLAMA_HOST`, `OPENAI_BASE_URL`,
/// `ANTHROPIC_BASE_URL`. The subscription rows are bound to one vendor's
/// identity and have no meaning at another address; OpenRouter is itself a
/// gateway, so a gateway in front of it is not a case this repository has.
///
/// `gemini` is here as of
/// `docs/decisions/2026-09-07-a-published-contract-is-enough-to-offer-a-row.md`.
/// It was the one implemented row left out of this array, and it was left out
/// for the reason that ADR rejects: no sanitized live recording exists. Its
/// absence was worse than the `openai` refusal it mirrored, because a row that
/// is not in this array has no refusal at all — `-m gemini/…` answered
/// "unknown provider", which is false about a mapping this build ships,
/// fixtures and all.
pub fn final_provider_rows(
    ollama: ProviderRow,
    openai: ProviderRow,
    anthropic: ProviderRow,
) -> [ProviderRow; FINAL_PROFILE_COUNT] {
    [
        ollama,
        openai,
        ProviderRow::openai_codex_default(),
        anthropic,
        ProviderRow::github_copilot_default(),
        ProviderRow::xai_oauth_default(),
        ProviderRow::kimi_coding_default(),
        ProviderRow::minimax_oauth_default(),
        ProviderRow::nous_default(),
        ProviderRow::openrouter_default(),
        ProviderRow::gemini_default(),
    ]
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

pub fn final_cost_expectations() -> [FinalCostExpectation; FINAL_PROFILE_COUNT] {
    final_provider_rows(
        ProviderRow::ollama_default(),
        ProviderRow::openai(),
        ProviderRow::anthropic_default(),
    )
    .map(|row| FinalCostExpectation {
        profile_id: row.id().to_string(),
        expected: row.cost_expectation(),
    })
}
