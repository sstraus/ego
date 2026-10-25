//! Production registry admission over fresh current-protocol MCP lifecycles.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ego_core::config::McpServerConfig;
use sha2::{Digest, Sha256};

use crate::operation::McpAcpOperationFactory;
use crate::operation::stdio_spawn_spec;
use crate::registry::McpToolSchemaAdmission;
use crate::{
    McpAcpEffects, McpAdmittedCandidate, McpCacheMetadata, McpCandidateAdmission,
    McpCandidateError, McpCandidateIdentity, McpCandidateInventory, McpCandidateRequest,
    McpCapabilities, McpClient, McpClientOptions, McpCollapsedBootstrap, McpContinuationOwner,
    McpDuplexEffectsFactory, McpFuture, McpHttpEffects, McpHttpOperationFactory,
    McpOperationFactory, McpStdioOperationFactory,
};

pub const MCP_DEFAULT_MAX_SSE_EVENT_SIZE: usize = 16 * 1024 * 1024;
pub const MCP_DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub(crate) struct McpRuntimeCandidateAdmission {
    http_management: Arc<dyn McpHttpEffects>,
    stdio_management: Arc<dyn McpDuplexEffectsFactory>,
    acp_management: Option<Arc<dyn McpAcpEffects>>,
    continuations: Arc<McpContinuationOwner>,
    schemas: Arc<dyn McpToolSchemaAdmission>,
    max_sse_event_size: usize,
    timeout: Duration,
}

impl McpRuntimeCandidateAdmission {
    pub(crate) fn new(
        http_management: Arc<dyn McpHttpEffects>,
        stdio_management: Arc<dyn McpDuplexEffectsFactory>,
        continuations: Arc<McpContinuationOwner>,
        schemas: Arc<dyn McpToolSchemaAdmission>,
        max_sse_event_size: usize,
        timeout: Duration,
        acp_management: Option<Arc<dyn McpAcpEffects>>,
    ) -> Self {
        Self {
            http_management,
            stdio_management,
            acp_management,
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
            McpServerConfig::Acp { .. } => Err(invalid_candidate(
                "ACP factory needs its retained connection",
            )),
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
        let acp_management = self.acp_management.clone();
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
                acp_management,
            );
            let options = runtime.options()?;
            if let McpServerConfig::Stdio(config) = request.config()
                && let Some(session) = runtime.stdio_management.session_stdio()
            {
                let spec = stdio_spawn_spec(request.admission().as_ref(), config, runtime.timeout)
                    .map_err(|error| invalid_candidate(error.to_string()))?;
                let (child, inventory) = session
                    .admit(request.name().clone(), spec, options)
                    .await
                    .map_err(candidate_error)?;
                let bootstrap = McpCollapsedBootstrap::detect(&inventory.tools)
                    .map_err(|error| invalid_candidate(error.to_string()))?;
                let factory = McpStdioOperationFactory::runtime_session(
                    Arc::clone(&runtime.continuations),
                    runtime.max_sse_event_size,
                    runtime.timeout,
                    bootstrap,
                    Arc::clone(&runtime.schemas),
                    session,
                    child,
                )
                .map_err(|error| invalid_candidate(error.to_string()))?;
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
                return McpAdmittedCandidate::new(
                    request.admission(),
                    McpCandidateIdentity::validated(request.name().as_str()),
                    McpCandidateInventory::new(
                        capabilities,
                        inventory.instructions,
                        inventory.tools,
                        inventory.resources,
                        inventory.prompts,
                        cache,
                        None,
                    ),
                    Arc::new(factory),
                )
                .map_err(|error| invalid_candidate(error.to_string()));
            }
            let server_admission = request.admission();
            if server_admission.oauth_session().is_some() {
                server_admission
                    .authorization_headers(
                        Some(Arc::clone(&runtime.http_management)),
                        options.deadline(),
                    )
                    .await
                    .map_err(|_| McpCandidateError::CredentialRequired)?;
            }
            let retained_acp = matches!(request.config(), McpServerConfig::Acp { .. });
            let mut acp_session = None;
            let result = async {
                let mut client = match request.config() {
                    McpServerConfig::Http(config) => McpClient::connect_http(
                        request.name().clone(),
                        config.clone(),
                        options,
                        Arc::new(crate::oauth::AuthorizedMcpHttpEffects {
                            admission: Arc::clone(&server_admission),
                            inner: Arc::clone(&runtime.http_management),
                        }),
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
                    McpServerConfig::Acp { server_id } => {
                        let effects = runtime
                            .acp_management
                            .clone()
                            .ok_or_else(|| invalid_candidate("ACP MCP channel is unavailable"))?;
                        let connection_id = effects
                            .connect(server_id.clone())
                            .await
                            .map_err(|error| invalid_candidate(error.to_string()))?;
                        acp_session = Some((connection_id.clone(), Arc::clone(&effects)));
                        McpClient::connect_acp(
                            request.name().clone(),
                            options,
                            connection_id,
                            effects,
                        )
                    }
                }
                .map_err(|error| invalid_candidate(error.to_string()))?;
                let inventory = async {
                    client.discover().await?;
                    client.inventory().await
                }
                .await;
                let inventory = inventory.map_err(|error| {
                    if server_admission
                        .oauth_session()
                        .is_some_and(|session| session.credential_required())
                        || (server_admission.oauth_session().is_some()
                            && matches!(
                                error.failure(),
                                crate::McpFailure::Credential | crate::McpFailure::Authorization
                            ))
                    {
                        McpCandidateError::CredentialRequired
                    } else {
                        candidate_error(error)
                    }
                })?;
                if let Some((connection_id, effects)) = &acp_session {
                    for uri in ["tuic://workspace", "tuic://inbox"] {
                        if !inventory
                            .resources
                            .iter()
                            .any(|resource| resource.uri == uri)
                        {
                            continue;
                        }
                        let params = serde_json::json!({"uri":uri}).as_object().cloned();
                        effects
                            .request(
                                connection_id.clone(),
                                "resources/subscribe".to_owned(),
                                params.clone(),
                            )
                            .await
                            .map_err(|error| invalid_candidate(error.to_string()))?;
                        let read = effects
                            .request(connection_id.clone(), "resources/read".to_owned(), params)
                            .await
                            .map_err(|error| invalid_candidate(error.to_string()))?;
                        let text = read["contents"]
                            .as_array()
                            .and_then(|contents| {
                                contents.iter().find(|content| content["uri"] == uri)
                            })
                            .and_then(|content| content["text"].as_str())
                            .ok_or_else(|| {
                                invalid_candidate(format!(
                                    "ACP MCP resource {uri} has no text content"
                                ))
                            })?;
                        effects.publish_resource(uri, text.to_owned());
                    }
                }
                let acp_client = if retained_acp {
                    Some(Arc::new(tokio::sync::Mutex::new(client)))
                } else {
                    client
                        .settle()
                        .await
                        .map_err(|error| invalid_candidate(error.to_string()))?;
                    None
                };
                let bootstrap = McpCollapsedBootstrap::detect(&inventory.tools)
                    .map_err(|error| invalid_candidate(error.to_string()))?;
                let factory = if let Some(client) = acp_client {
                    Arc::new(
                        McpAcpOperationFactory::runtime(
                            Arc::clone(&runtime.continuations),
                            runtime.max_sse_event_size,
                            runtime.timeout,
                            bootstrap,
                            Arc::clone(&runtime.schemas),
                            client,
                        )
                        .map_err(|error| invalid_candidate(error.to_string()))?,
                    ) as Arc<dyn McpOperationFactory>
                } else {
                    runtime.operation_factory(request.config(), bootstrap)?
                };
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
                        inventory.instructions,
                        inventory.tools,
                        inventory.resources,
                        inventory.prompts,
                        cache,
                        None,
                    ),
                    factory,
                )
                .map_err(|error| invalid_candidate(error.to_string()))
            }
            .await;
            match result {
                Err(error) => {
                    if let Some((connection_id, effects)) = acp_session
                        && effects.disconnect(connection_id).await.is_err()
                    {
                        return Err(invalid_candidate(
                            "ACP MCP candidate failed and its host connection could not be closed",
                        ));
                    }
                    Err(error)
                }
                admitted => admitted,
            }
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

fn candidate_error(error: crate::McpError) -> McpCandidateError {
    match error.denied_executable() {
        Some(denial) => McpCandidateError::ExecutableDenied {
            denial: denial.clone(),
        },
        None => invalid_candidate(error.to_string()),
    }
}
