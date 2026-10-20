//! Catalogue policy from captured provider documentation, 2026-10-16.
//! See ARCHITECTURE.md Unknown support fails closed.

use ego_core::budget::PricingQuote;
use ego_core::cost::Amount;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    #[default]
    Standard,
    Fast,
}

impl ServiceTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Fast => "fast",
        }
    }

    pub fn validate(self, slug: &str) -> Result<(), String> {
        if self == Self::Fast && fast_support(slug).is_none() {
            return Err(format!(
                "fast is unavailable for {slug}: the catalogue has no verified fast tier and price; use standard"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FastSupport {
    pub token_price_multiplier: u32,
    /// Included subscription allowance is not a currency charge.
    pub included_usage_multiplier: Option<&'static str>,
    pub source: &'static str,
    pub account_requirement: &'static str,
    pub wire_field: &'static str,
    pub wire_value: &'static str,
    pub beta_header: Option<&'static str>,
}

/// Shared by catalogue consumers, selection validation, pricing, and transport.
pub fn fast_support(slug: &str) -> Option<FastSupport> {
    let (provider, model) = slug.split_once('/')?;
    let openai_model = matches!(
        model,
        "gpt-6-astra" | "gpt-6.1-sol" | "gpt-6-sol" | "gpt-6-luna"
    );
    let anthropic_model = matches!(
        model,
        "claude-opus-5-5" | "claude-opus-5" | "claude-opus-4-8"
    );
    match provider {
        "openai" | "openai-codex" if openai_model => Some(FastSupport {
            token_price_multiplier: 2,
            included_usage_multiplier: (provider == "openai-codex").then_some("2.5"),
            source: if provider == "openai-codex" {
                "https://learn.chatgpt.com/docs/agent-configuration/speed"
            } else {
                "https://developers.openai.com/api/docs/guides/fast-mode"
            },
            account_requirement: "subject to account, model and regional availability",
            wire_field: "service_tier",
            wire_value: "fast",
            beta_header: None,
        }),
        "anthropic" if anthropic_model => Some(FastSupport {
            token_price_multiplier: 2,
            included_usage_multiplier: None,
            source: "https://platform.claude.com/docs/en/build-with-claude/fast-mode",
            account_requirement: if model == "claude-opus-5-5" {
                "Start, Build or Scale usage tier"
            } else {
                "research preview access required"
            },
            wire_field: "speed",
            wire_value: "fast",
            beta_header: (model != "claude-opus-5-5").then_some("fast-mode-2026-02-01"),
        }),
        "openrouter" => {
            model
                .strip_prefix("openai/")
                .filter(|m| {
                    matches!(
                        *m,
                        "gpt-6-astra" | "gpt-6.1-sol" | "gpt-6-sol" | "gpt-6-luna"
                    )
                })
                .or_else(|| {
                    model.strip_prefix("anthropic/").filter(|m| {
                        matches!(*m, "claude-opus-5.5" | "claude-opus-5" | "claude-opus-4.8")
                    })
                })?;
            Some(FastSupport {
                token_price_multiplier: 2,
                included_usage_multiplier: None,
                source: "https://openrouter.ai/docs/guides/features/service-tiers",
                account_requirement: "verified upstream fast endpoint; provider-reported settlement, routing may fall back to standard",
                wire_field: "service_tier",
                wire_value: "priority",
                beta_header: None,
            })
        }
        _ => None,
    }
}

/// Multiply token rates only. Search/tool tariffs are not token prices.
/// The same quote bounds and settles the generation.
pub fn fast_quote(quote: &PricingQuote, support: &FastSupport) -> Result<PricingQuote, String> {
    let mut quote = quote.clone();
    let multiplier =
        Amount::new(support.token_price_multiplier.into(), 0).map_err(|error| error.to_string())?;
    for rate in [
        &mut quote.uncached_input,
        &mut quote.cache_read,
        &mut quote.cache_write,
        &mut quote.cache_write_extended,
        &mut quote.output,
    ]
    .into_iter()
    .flatten()
    {
        *rate = rate
            .checked_mul(&multiplier)
            .ok_or("fast token price overflow")?;
    }
    quote.revision.0.push_str("+fast-2026-10-16");
    Ok(quote)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_contract_catches_support_inferred_from_provider_name() {
        for slug in [
            "openai/unknown",
            "openai-codex/codex-auto-review",
            "anthropic/claude-sonnet-5-5",
            "openrouter/unknown/model",
            "gemini/gemini-3.7-pro",
        ] {
            assert!(
                ServiceTier::Fast
                    .validate(slug)
                    .unwrap_err()
                    .contains("no verified fast tier")
            );
            assert!(ServiceTier::Standard.validate(slug).is_ok());
        }
        for slug in [
            "openai/gpt-6.1-sol",
            "openai-codex/gpt-6.1-sol",
            "anthropic/claude-opus-5-5",
            "openrouter/openai/gpt-6.1-sol",
        ] {
            assert_eq!(fast_support(slug).unwrap().token_price_multiplier, 2);
        }
        assert_eq!(
            fast_support("openai-codex/gpt-6.1-sol")
                .unwrap()
                .included_usage_multiplier,
            Some("2.5")
        );
    }

    #[test]
    fn fast_contract_catches_missing_cache_multiplier_or_multiplied_search_tariff() {
        let quote = PricingQuote {
            revision: ego_core::cost::PricingRevision("captured-standard".into()),
            currency: ego_core::cost::CurrencyCode::new("USD").unwrap(),
            uncached_input: Some("0.000002".parse().unwrap()),
            cache_read: Some("0.0000001".parse().unwrap()),
            cache_write: Some("0.0000025".parse().unwrap()),
            cache_write_extended: Some("0.000004".parse().unwrap()),
            output: Some("0.00001".parse().unwrap()),
            web_search: Some("0.01".parse().unwrap()),
        };
        let fast = fast_quote(&quote, &fast_support("openai/gpt-6.1-sol").unwrap()).unwrap();
        assert_eq!(fast.uncached_input.unwrap().to_string(), "0.000004");
        assert_eq!(fast.cache_read.unwrap().to_string(), "0.0000002");
        assert_eq!(fast.cache_write.unwrap().to_string(), "0.000005");
        assert_eq!(fast.cache_write_extended.unwrap().to_string(), "0.000008");
        assert_eq!(fast.output.unwrap().to_string(), "0.00002");
        assert_eq!(fast.web_search, quote.web_search);
    }
    #[test]
    fn fast_contract_catches_openrouter_slug_or_tariff_drift_from_captured_endpoints() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/service-tier");
        for model in [
            "openai-gpt-6-astra",
            "openai-gpt-6.1-sol",
            "openai-gpt-6-sol",
            "openai-gpt-6-luna",
            "anthropic-claude-opus-5-5",
            "anthropic-claude-opus-5",
            "anthropic-claude-opus-4-8",
        ] {
            let capture: serde_json::Value = serde_json::from_slice(
                &std::fs::read(root.join(format!("{model}-endpoints.json"))).unwrap(),
            )
            .unwrap();
            let id = capture["data"]["id"].as_str().unwrap();
            let support = fast_support(&format!("openrouter/{id}"))
                .expect("captured canonical model slug must be supported");
            let endpoints = capture["data"]["endpoints"].as_array().unwrap();
            let fast = endpoints
                .iter()
                .find(|endpoint| endpoint["service_tier"] == "priority")
                .unwrap();
            let base_tag = fast["tag"].as_str().unwrap().strip_suffix("/fast").unwrap();
            let standard = endpoints
                .iter()
                .find(|endpoint| endpoint["tag"] == base_tag)
                .unwrap();
            for field in [
                "prompt",
                "completion",
                "input_cache_read",
                "input_cache_write",
            ] {
                if let Some(base) = standard["pricing"][field].as_str() {
                    let base: Amount = base.parse().unwrap();
                    let premium: Amount = fast["pricing"][field].as_str().unwrap().parse().unwrap();
                    assert_eq!(
                        base.checked_mul(
                            &Amount::new(u128::from(support.token_price_multiplier), 0).unwrap()
                        )
                        .unwrap(),
                        premium,
                        "{id} {field}"
                    );
                }
            }
        }
    }
}
