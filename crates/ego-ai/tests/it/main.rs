//! The one integration-test binary of `ego-ai`.
//!
//! Every file in `tests/` used to be its own binary, and each one linked the
//! whole dependency graph. One binary links it once. Each former binary is a
//! module here, so `cargo nextest run -p ego-ai --test it -E 'test(/^<file>::/)'`
//! selects what `--test <file>` selected before.

#[path = "../profiles/mod.rs"]
mod profiles;

#[path = "../support/mod.rs"]
mod support;

mod anthropic_messages;
mod audio_profiles;
mod catalogue;
mod conformance;
mod context_overflow;
mod current_provider_contract;
mod deprecated_spellings;
mod endpoint_agreement;
mod gemini_interactions;
mod generator;
mod google_adc;
mod hosted_search;
mod http;
mod images_profiles;
mod invariants;
mod login_headers;
mod model_catalogue;
mod oauth;
mod ollama_ceiling;
mod ollama_reasoning;
mod ollama_tool_call;
mod openai_codex_responses;
mod openai_completions;
mod openai_responses;
mod partner_messages;
mod pricing_decimal;
mod pricing_fetch;
mod pricing_table;
mod prompt_cache;
mod provider_cost;
mod provider_excerpt_critic;
mod provider_failure_class;
mod provider_security;
mod request_target;
mod skill_schema;
mod subscription_contracts;
mod vendor_client_identity;
mod xai_responses;
