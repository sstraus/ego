//! Production registry admission over fresh current-protocol MCP lifecycles.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ego_core::config::McpServerConfig;
use sha2::{Digest, Sha256};

use crate::operation::stdio_spawn_spec;
use crate::registry::McpToolSchemaAdmission;
use crate::{
    McpAdmittedCandidate, McpCacheMetadata, McpCandidateAdmission, McpCandidateError,
    McpCandidateIdentity, McpCandidateInventory, McpCandidateRequest, McpCapabilities, McpClient,
    McpClientOptions, McpCollapsedBootstrap, McpDuplexEffectsFactory, McpFuture, McpHttpEffects,
    McpHttpOperationFactory, McpInputRequiredRecorder, McpOperationFactory,
    McpStdioOperationFactory,
};

pub const MCP_DEFAULT_MAX_SSE_EVENT_SIZE: usize = 16 * 1024 * 1024;
pub const MCP_DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub(crate) struct McpRuntimeCandidateAdmission {
    http_management: Arc<dyn McpHttpEffects>,
    stdio_management: Arc<dyn McpDuplexEffectsFactory>,
    continuations: Arc<dyn McpInputRequiredRecorder>,
    schemas: Arc<dyn McpToolSchemaAdmission>,
    max_sse_event_size: usize,
    timeout: Duration,
}

impl McpRuntimeCandidateAdmission {
    pub(crate) fn new(
        http_management: Arc<dyn McpHttpEffects>,
        stdio_management: Arc<dyn McpDuplexEffectsFactory>,
        continuations: Arc<dyn McpInputRequiredRecorder>,
        schemas: Arc<dyn McpToolSchemaAdmission>,
        max_sse_event_size: usize,
        timeout: Duration,
    ) -> Self {
        Self {
            http_management,
            stdio_management,
            continuations,
            schemas,
            max_sse_event_size,
            timeout,
        }
    }

    fn options(&self) -> Result<McpClientOptions, McpCandidateError> {
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .ok_or_else(|| invalid_candidate("MCP candidate admission deadline overflowed"))?;
        McpClientOptions::new(self.max_sse_event_size, deadline)
            .map_err(|error| invalid_candidate(error.to_string()))
    }

    fn operation_factory(
        &self,
        config: &McpServerConfig,
        bootstrap: Option<McpCollapsedBootstrap>,
    ) -> Result<Arc<dyn McpOperationFactory>, McpCandidateError> {
        match config {
            McpServerConfig::Http(_) => McpHttpOperationFactory::runtime(
                Arc::clone(&self.http_management),
                Arc::clone(&self.continuations),
                self.max_sse_event_size,
                self.timeout,
                bootstrap,
                Arc::clone(&self.schemas),
            )
            .map(|factory| Arc::new(factory) as Arc<dyn McpOperationFactory>)
            .map_err(|error| invalid_candidate(error.to_string())),
            McpServerConfig::Stdio(_) => McpStdioOperationFactory::runtime(
                Arc::clone(&self.continuations),
                self.max_sse_event_size,
                self.timeout,
                bootstrap,
                Arc::clone(&self.schemas),
            )
            .map(|factory| Arc::new(factory) as Arc<dyn McpOperationFactory>)
            .map_err(|error| invalid_candidate(error.to_string())),
        }
    }
}

impl McpCandidateAdmission for McpRuntimeCandidateAdmission {
    fn admit(
        &self,
        request: McpCandidateRequest,
    ) -> McpFuture<'static, Result<McpAdmittedCandidate, McpCandidateError>> {
        let http_management = Arc::clone(&self.http_management);
        let stdio_management = Arc::clone(&self.stdio_management);
        let continuations = Arc::clone(&self.continuations);
        let schemas = Arc::clone(&self.schemas);
        let max_sse_event_size = self.max_sse_event_size;
        let timeout = self.timeout;
        Box::pin(async move {
            let runtime = Self::new(
                http_management,
                stdio_management,
                continuations,
                schemas,
                max_sse_event_size,
                timeout,
            );
            let options = runtime.options()?;
            let server_admission = request.admission();
            let mut client = match request.config() {
                McpServerConfig::Http(config) => McpClient::connect_http(
                    request.name().clone(),
                    config.clone(),
                    options,
                    Arc::clone(&runtime.http_management),
                ),
                McpServerConfig::Stdio(config) => {
                    stdio_spawn_spec(server_admission.as_ref(), config, runtime.timeout)
                        .and_then(|spec| {
                            runtime
                                .stdio_management
                                .create(request.name().clone(), spec)
                        })
                        .and_then(|effects| {
                            McpClient::connect_stdio(request.name().clone(), options, effects)
                        })
                }
            }
            .map_err(|error| invalid_candidate(error.to_string()))?;
            let inventory = async {
                client.discover().await?;
                client.inventory().await
            }
            .await;
            let settlement = client.settle().await;
            let inventory = inventory.map_err(|error| invalid_candidate(error.to_string()))?;
            settlement.map_err(|error| invalid_candidate(error.to_string()))?;
            let bootstrap = McpCollapsedBootstrap::detect(&inventory.tools)
                .map_err(|error| invalid_candidate(error.to_string()))?;
            let factory = runtime.operation_factory(request.config(), bootstrap)?;
            let capabilities = McpCapabilities::new(
                inventory.capabilities.tools.is_some(),
                inventory.capabilities.resources.is_some(),
                inventory.capabilities.prompts.is_some(),
            );
            let cache = cache_metadata(
                &inventory.tools,
                &inventory.resources,
                &inventory.prompts,
                inventory.expires_at,
            )?;
            McpAdmittedCandidate::new(
                server_admission,
                McpCandidateIdentity::validated(request.name().as_str()),
                McpCandidateInventory::new(
                    capabilities,
                    inventory.tools,
                    inventory.resources,
                    inventory.prompts,
                    cache,
                    None,
                ),
                factory,
            )
            .map_err(|error| invalid_candidate(error.to_string()))
        })
    }
}

fn cache_metadata(
    tools: &[ego_core::generator::types::ToolDescriptor],
    resources: &[rmcp::model::Resource],
    prompts: &[rmcp::model::Prompt],
    expires_at: Option<Instant>,
) -> Result<McpCacheMetadata, McpCandidateError> {
    if tools.is_empty() && resources.is_empty() && prompts.is_empty() && expires_at.is_none() {
        return Ok(McpCacheMetadata::default());
    }
    let encoded = serde_json::to_vec(&(tools, resources, prompts))
        .map_err(|error| invalid_candidate(error.to_string()))?;
    let source_revision = format!("sha256:{:x}", Sha256::digest(encoded));
    Ok(McpCacheMetadata::new(Some(source_revision), expires_at))
}

fn invalid_candidate(cause: impl Into<String>) -> McpCandidateError {
    McpCandidateError::Invalid {
        cause: cause.into(),
    }
}
