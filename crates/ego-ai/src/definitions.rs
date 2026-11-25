//! One validated path from provider definitions to the rows the catalogue
//! holds.
//!
//! A [`Catalogue`](crate::catalogue::Catalogue) keys its rows by provider id.
//! Collecting rows into that map keeps the last row of a repeated id and
//! drops the first one without a word, so two definitions of one provider
//! become one silent override. Every inventory goes through
//! [`provider_rows`] instead, which refuses the second definition and names
//! where both came from.

use std::collections::BTreeMap;
use std::fmt;

use crate::provider::ProviderRow;

/// One provider row and the place that declared it.
///
/// The source is the text a duplicate refusal shows: the provider id alone
/// cannot tell two definitions of the same id apart.
#[derive(Debug, Clone)]
pub struct ProviderDefinition {
    source: String,
    row: ProviderRow,
}

impl ProviderDefinition {
    pub fn new(source: impl Into<String>, row: ProviderRow) -> Self {
        Self {
            source: source.into(),
            row,
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn row(&self) -> &ProviderRow {
        &self.row
    }
}

/// Two definitions of one provider id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateProvider {
    pub id: String,
    pub first: String,
    pub second: String,
}

impl fmt::Display for DuplicateProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "provider `{}` is defined twice: by {} and by {}",
            self.id, self.first, self.second
        )
    }
}

impl std::error::Error for DuplicateProvider {}

/// The rows of `definitions`, in their order, when every provider id is
/// unique.
pub fn provider_rows(
    definitions: impl IntoIterator<Item = ProviderDefinition>,
) -> Result<Vec<ProviderRow>, DuplicateProvider> {
    let mut sources: BTreeMap<String, String> = BTreeMap::new();
    let mut rows = Vec::new();
    for definition in definitions {
        let id = definition.row.id().to_owned();
        if let Some(first) = sources.get(&id) {
            return Err(DuplicateProvider {
                id,
                first: first.clone(),
                second: definition.source,
            });
        }
        sources.insert(id, definition.source);
        rows.push(definition.row);
    }
    Ok(rows)
}

pub(crate) fn instance_wire(
    contract: ego_core::config::ProviderModelContract,
) -> &'static dyn crate::wire::WireFormat {
    use ego_core::config::ProviderModelContract as Contract;
    match contract {
        Contract::ChatStandard => &crate::wire::openai_completions::OpenAiCompletions,
        Contract::ChatMaxTokens => &crate::wire::chat_max_tokens::ChatMaxTokens,
        Contract::Responses => &crate::wire::openai_responses::OpenAiResponses,
        Contract::Messages => &crate::wire::anthropic_messages::AnthropicMessages,
        Contract::GeminiInteractions => &crate::wire::gemini_interactions::GeminiInteractions,
        Contract::Deepseek => &crate::wire::deepseek::DeepSeek,
        Contract::BedrockConverse => &crate::wire::bedrock_converse::BedrockConverse,
        Contract::BedrockChat => &crate::wire::chat_max_tokens::BEDROCK_CHAT,
        Contract::Zai => &crate::wire::zai::Zai,
        Contract::KimiApi => &crate::wire::kimi_api::KimiApi,
        Contract::KimiResponses => &crate::wire::kimi_responses::KimiResponses,
        Contract::MistralConversations => &crate::wire::mistral_conversations::MistralConversations,
    }
}

/// Exact configured contracts use the existing resolver port, not a host branch.
#[derive(Debug)]
pub(crate) struct ConfiguredInstanceResolver;
pub(crate) static INSTANCE_RESOLVER: ConfiguredInstanceResolver = ConfiguredInstanceResolver;

impl crate::resolver::ModelResolver for ConfiguredInstanceResolver {
    fn supplied_request_mapping(
        &self,
        at: &crate::resolver::StaticResolution<'_>,
    ) -> Result<Option<crate::generator::StaticRequestProfile>, crate::catalogue::ResolveError>
    {
        let binding = at.catalogue.instance_binding(at.slug)?.ok_or_else(|| {
            crate::catalogue::ResolveError::HostedModelNotConfigured {
                slug: at.slug.to_string(),
            }
        })?;
        at.catalogue
            .static_request_profile_with_mapping(at.slug, instance_wire(binding.contract))
            .map(Some)
    }

    fn resolve<'a>(
        &'a self,
        at: &'a crate::resolver::Resolution<'a>,
    ) -> futures::future::BoxFuture<
        'a,
        Result<crate::catalogue::ResolvedModel, crate::catalogue::ResolveError>,
    > {
        Box::pin(std::future::ready((|| {
            let binding = at.catalogue.instance_binding(at.slug)?.ok_or_else(|| {
                crate::catalogue::ResolveError::HostedModelNotConfigured {
                    slug: at.slug.to_string(),
                }
            })?;
            at.catalogue
                .resolve_hosted(at.slug, instance_wire(binding.contract))
        })()))
    }

    fn enumerate<'a>(
        &'a self,
        at: &'a crate::resolver::Enumeration<'a>,
    ) -> futures::future::BoxFuture<'a, Result<crate::catalogue::ModelDiscoveryReport, String>>
    {
        Box::pin(std::future::ready(
            at.catalogue.enumerate_hosted(at.row.id(), None),
        ))
    }
}
