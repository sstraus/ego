//! API-key profiles for the bounded native reasoning Chat contracts.
//! Model capabilities are supplied explicitly, never inferred from a row.

use url::Url;

use super::{
    CostExpectation, CredentialStrategy, PriceTable, Promotion, ProviderRow, admitted_base,
};
use crate::resolver::Supplied;
use crate::wire::{kimi_api::KimiApi, mistral_conversations, zai::Zai};

pub const ZAI: &str = "zai";
pub const ZAI_BASE: &str = "https://api.z.ai/api/paas/v4/";
pub const ZAI_KEY_VAR: &str = "ZAI_API_KEY";
pub const KIMI_API: &str = "kimi-api";
pub const KIMI_API_BASE: &str = "https://api.moonshot.ai/v1/";
pub const KIMI_API_KEY_VAR: &str = "MOONSHOT_API_KEY";

static ZAI_SUPPLIED: Supplied = Supplied::new(&Zai);
static KIMI_API_SUPPLIED: Supplied = Supplied::new(&KimiApi);
static MISTRAL_SUPPLIED: Supplied = Supplied::new(&mistral_conversations::MistralConversations);

impl ProviderRow {
    /// Standard Z.ai API-key root, not the separate coding subscription.
    /// No default model, model facts, discovery or price is supplied here.
    pub fn zai_default() -> Self {
        let (base, http_profile) = admitted_base(
            ZAI,
            Url::parse(ZAI_BASE).expect("a constant this file controls"),
        );
        Self {
            id: ZAI.to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            price_table: PriceTable::Unpriced,
            cost_expectation: CostExpectation::Positive,
            resolver: &ZAI_SUPPLIED,
            credential: CredentialStrategy::BearerFromEnv {
                var: ZAI_KEY_VAR.to_string(),
            },
            cost: None,
            promotion: Promotion::Selectable,
        }
    }

    /// Mistral's API-key root. Models and prices require explicit facts.
    pub fn mistral_default() -> Self {
        let id = "mistral";
        let (base, http_profile) = admitted_base(
            id,
            Url::parse(mistral_conversations::BASE).expect("a constant this wire controls"),
        );
        Self {
            id: id.into(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            price_table: PriceTable::Unpriced,
            cost_expectation: CostExpectation::Positive,
            resolver: &MISTRAL_SUPPLIED,
            credential: CredentialStrategy::BearerFromEnv {
                var: "MISTRAL_API_KEY".into(),
            },
            cost: None,
            promotion: Promotion::Selectable,
        }
    }

    /// Moonshot's API-key root, distinct from the managed coding subscription.
    /// Only explicit reviewed/operator model facts can resolve a model here.
    pub fn kimi_api_default() -> Self {
        let (base, http_profile) = admitted_base(
            KIMI_API,
            Url::parse(KIMI_API_BASE).expect("a constant this file controls"),
        );
        Self {
            id: KIMI_API.to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            price_table: PriceTable::Unpriced,
            cost_expectation: CostExpectation::Positive,
            resolver: &KIMI_API_SUPPLIED,
            credential: CredentialStrategy::BearerFromEnv {
                var: KIMI_API_KEY_VAR.to_string(),
            },
            cost: None,
            promotion: Promotion::Selectable,
        }
    }
}
