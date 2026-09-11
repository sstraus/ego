//! behavioral contract for regression case provider security and conformance.

use std::sync::Arc;

use ego_ai::catalogue;
use ego_ai::provider::{CredentialStrategy, Credentials, MissingCredential, ProviderRow, Secret};
use ego_ai::wire::provider_cost::{OpenRouterUsd, ProviderCost, XaiUsdTicks};
use ego_core::budget::Metering;
use ego_core::credentials::SecretMaterial;
use ego_core::permit::HttpHeader;

const LIVE_SECRET: &str = "secret-material-that-must-not-escape";

struct CredentialsFixture;

impl Credentials for CredentialsFixture {
    fn secret(&self, _name: &str) -> Result<Secret, MissingCredential> {
        Ok(LIVE_SECRET.into())
    }
}

#[test]
fn one_resolved_owner_is_redacted_and_secret_headers_are_typed() {
    let credential = CredentialStrategy::BearerFromEnv { var: "KEY".into() }
        .resolve(&CredentialsFixture)
        .unwrap();
    let debug = format!("{credential:?}");
    assert!(!debug.contains(LIVE_SECRET));
    assert!(!debug.contains("Bearer"));

    let material = SecretMaterial::new(LIVE_SECRET.as_bytes().to_vec());
    let header = HttpHeader::secret("authorization", material).unwrap();
    assert!(header.is_secret());
    assert_eq!(header.name(), "authorization");
    assert_eq!(header.value(), LIVE_SECRET.as_bytes());
    assert!(!format!("{header:?}").contains(LIVE_SECRET));
}

#[test]
fn current_provider_inventory_excludes_the_deprecated_xai_api_key_candidate() {
    let current = [
        ProviderRow::ollama_default(),
        ProviderRow::openai(),
        ProviderRow::openai_codex_default(),
        ProviderRow::anthropic_default(),
        ProviderRow::github_copilot_default(),
        ProviderRow::xai_oauth_default(),
        ProviderRow::kimi_coding_default(),
        ProviderRow::minimax_oauth_default(),
        ProviderRow::nous_default(),
        ProviderRow::openrouter_default(),
    ];
    assert!(!current.iter().any(|row| row.id() == "xai"));
}

#[test]
fn xai_oauth_retains_responses_mapping_and_subscription_settlement() {
    let row = ProviderRow::xai_oauth_default();
    assert_eq!(row.id(), "xai-oauth");
    assert!(row.base().as_str().ends_with("/v1/"));
    let resolved = row.credential.resolve(&CredentialsFixture).unwrap();
    assert!(resolved.base_override().is_none());
}

#[test]
fn supplied_metering_uses_provider_reported_cost_without_provider_id_policy() {
    let decoders: Vec<Arc<dyn ProviderCost>> = vec![
        Arc::new(XaiUsdTicks::new("087-test")),
        Arc::new(OpenRouterUsd::new("087-test")),
    ];
    for decoder in decoders {
        let metering = catalogue::provider_reported(decoder.as_ref());
        assert!(matches!(metering, Metering::Currency { .. }));
    }
}

#[test]
fn final_cost_expectations_are_exhaustive_and_positive_or_no_charge() {
    let expectations = ego_ai::conformance::final_cost_expectations();
    assert_eq!(expectations.len(), ego_ai::conformance::FINAL_PROFILE_COUNT);
    for expectation in expectations {
        assert!(!expectation.profile_id().is_empty());
        let expected = expectation.expected();
        assert!(expectation.accepts(expected));
    }
}
