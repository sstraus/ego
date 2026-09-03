//! Final provider inventory and the cost behavior each profile declares.

use crate::provider::{CostExpectation, ProviderRow};

pub const FINAL_PROFILE_COUNT: usize = 10;

pub fn final_provider_rows(
    ollama: ProviderRow,
    openai: ProviderRow,
) -> [ProviderRow; FINAL_PROFILE_COUNT] {
    [
        ollama,
        openai,
        ProviderRow::openai_codex_default(),
        ProviderRow::anthropic_default(),
        ProviderRow::github_copilot_default(),
        ProviderRow::xai_oauth_default(),
        ProviderRow::kimi_coding_default(),
        ProviderRow::minimax_oauth_default(),
        ProviderRow::nous_default(),
        ProviderRow::openrouter_default(),
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
    final_provider_rows(ProviderRow::ollama_default(), ProviderRow::openai()).map(|row| {
        FinalCostExpectation {
            profile_id: row.id().to_string(),
            expected: row.cost_expectation(),
        }
    })
}
