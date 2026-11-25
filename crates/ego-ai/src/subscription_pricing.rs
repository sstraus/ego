//! Standard API equivalents, not subscription charges or budget rates.
//! Source: official model pages; see the design notes

use ego_core::budget::PricingQuote;
use ego_core::cost::{CurrencyCode, EquivalentPricing, PricingRevision};

pub(crate) fn standard(slug: &str) -> Option<Box<EquivalentPricing>> {
    let model = slug.strip_prefix("openai-codex/")?;
    let (input, read, write, output) = match model {
        "gpt-6-astra" => ("0.00001", "0.000001", "0.0000125", "0.00005"),
        "gpt-6-luna" => ("0.0000001", "0.00000001", "0.000000125", "0.0000005"),
        "gpt-6-sol" => ("0.000002", "0.0000002", "0.0000025", "0.00001"),
        "gpt-6.1-sol" | "gpt-6.2-sol" => ("0.000002", "0.0000001", "0.0000025", "0.00001"),
        _ => return None,
    };
    let (revision_date, extended_write) = if model == "gpt-6.2-sol" {
        ("2026-11-20", Some("0.000005"))
    } else {
        ("2026-10-16", None)
    };
    Some(Box::new(EquivalentPricing {
        model: slug.to_owned(),
        service_tier: "standard".to_owned(),
        quote: PricingQuote {
            web_search: None,
            revision: PricingRevision(format!(
                "https://developers.openai.com/api/docs/models/{model}#standard-{revision_date}"
            )),
            currency: CurrencyCode::new("USD").expect("USD is a currency code"),
            uncached_input: Some(input.parse().expect("verified decimal rate")),
            cache_read: Some(read.parse().expect("verified decimal rate")),
            cache_write: Some(write.parse().expect("verified decimal rate")),
            cache_write_extended: extended_write
                .map(|rate| rate.parse().expect("verified decimal rate")),
            output: Some(output.parse().expect("verified decimal rate")),
            long_context: Vec::new(),
        },
        long_context_threshold: 272_000,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::cost::SubscriptionEquivalent;
    use ego_core::state::Usage;

    #[test]
    fn official_standard_prices_cover_each_tier_and_cache_categories() {
        // Independent decimal totals from the captured official per-million prices.
        for (model, expected) in [
            ("gpt-6-astra", "122"),
            ("gpt-6-luna", "1.22"),
            ("gpt-6-sol", "24.4"),
            ("gpt-6.1-sol", "24.2"),
        ] {
            let price = standard(&format!("openai-codex/{model}")).unwrap();
            let usage = Usage {
                uncached_input_tokens: 1_000_000,
                cache_read_tokens: 1_000_000,
                cache_write_tokens: 1_000_000,
                output_tokens: 1_000_000,
                ..Usage::IDENTITY
            };
            let estimate = price.estimate(&usage).unwrap();
            let SubscriptionEquivalent::V1 {
                amount,
                model: recorded,
                service_tier,
                source,
            } = *estimate;
            assert_eq!(amount.amount.to_string(), expected, "{model}");
            assert_eq!(recorded, format!("openai-codex/{model}"));
            assert_eq!(service_tier, "standard");
            assert!(source.0.contains(model));
        }
    }

    // Catches an absent Sol 6.2 quote or short-context pricing above 272K.
    #[test]
    fn sol_62_standard_estimate_prices_extended_writes_and_long_context() {
        let price = standard("openai-codex/gpt-6.2-sol").expect("published Standard API rates");
        for (input, expected) in [(269_000, "0.5576"), (269_001, "1.109204")] {
            let estimate = price
                .estimate(&Usage {
                    uncached_input_tokens: input,
                    cache_read_tokens: 1000,
                    cache_write_tokens: 1000,
                    cache_write_extended_tokens: 1000,
                    output_tokens: 1200,
                    ..Usage::IDENTITY
                })
                .unwrap();
            let SubscriptionEquivalent::V1 { amount, .. } = *estimate;
            assert_eq!(amount.amount.to_string(), expected);
        }
    }

    #[test]
    fn long_context_threshold_is_strict_and_prices_the_whole_request() {
        let price = standard("openai-codex/gpt-6.1-sol").unwrap();
        for (input, expected) in [(272_000, "0.54401"), (272_001, "1.088019")] {
            let estimate = price
                .estimate(&Usage {
                    uncached_input_tokens: input,
                    output_tokens: 1,
                    ..Usage::IDENTITY
                })
                .unwrap();
            let SubscriptionEquivalent::V1 { amount, .. } = *estimate;
            assert_eq!(amount.amount.to_string(), expected);
        }
    }

    #[test]
    fn unsupported_models_and_unpriced_cache_retention_never_get_zero_quotes() {
        assert!(standard("openai-codex/unknown").is_none());
        assert!(standard("openai/gpt-6.1-sol").is_none());
        assert!(
            standard("openai-codex/gpt-6.1-sol")
                .unwrap()
                .estimate(&Usage {
                    cache_write_extended_tokens: 1,
                    ..Usage::IDENTITY
                })
                .is_none()
        );
    }
}
