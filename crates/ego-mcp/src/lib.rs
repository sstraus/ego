//! MCP protocol ownership and guarded transport adapters.
//!
//! Outbound clients default to stateless 2026-07-28; explicit legacy SSE uses
//! the 2024-11-05 initialize lifecycle. The inbound stdio server
//! negotiates its explicit handshake-era revision set through `rmcp`.

mod admission;
mod client;
mod dynamic;
mod effects;
mod error;
mod hook;
mod http;
mod oauth;
mod operation;
mod registry;
mod run_server;
mod runtime;
mod server;
mod state;
mod stdio;

pub use admission::{
    McpEnvironmentBinding, McpHeaderBinding, McpServerAdmission, McpTransportMaterial,
};
pub use client::{
    MCP_PROTOCOL_REVISION, McpClient, McpClientCancellation, McpClientOptions, McpClientSettlement,
};
pub use dynamic::{
    McpCapturedTool, McpCollapsedBootstrap, McpCollapsedCatalogue, McpOperation,
    McpOperationFactory,
};
pub use effects::{
    McpAcpEffects, McpDuplexEffects, McpDuplexEffectsFactory, McpFuture, McpHttpEffects,
    McpHttpEnd, McpHttpPurpose, McpHttpSettlement, McpPreparedHttpParts, McpRequestId,
    PreparedMcpHttpCall,
};
pub use ego_core::generator::attempt::DeliveryCertainty;
pub use error::{McpError, McpFailure};
pub use hook::{
    McpHookAdapter, McpHookAdapterError, McpHookExecution, McpHookFailure, McpHookSettlement,
};
pub use http::{McpHttpBody, McpHttpResponse};
pub use oauth::{
    McpAuthorizationInteraction, McpClientMetadata, McpCredentialBundle, McpLoginRequest,
    McpLoginResult, McpOAuthCallback, McpOAuthInteraction, McpOAuthRefreshLease,
    McpOAuthRefreshLock, McpOAuthSession, McpPkceContext, McpPkceMethod, McpRegistrationOptions,
    McpTokenRefreshRequest, McpTokenRefreshResult, login, refresh_token,
};
pub use operation::{
    McpCaptureResolver, McpContinuationOwner, McpForeignQuestionPort, McpHttpOperationFactory,
    McpResumeAdmission, McpStdioOperationFactory,
};
pub use registry::{
    McpAdmittedCandidate, McpAuthorizationStatus, McpCacheFreshness, McpCacheMetadata,
    McpCandidateAdmission, McpCandidateError, McpCandidateIdentity, McpCandidateInventory,
    McpCandidateRequest, McpCapabilities, McpCapturedContextOperation, McpCapturedContextSource,
    McpCapturedServer, McpRegistryCandidate, McpRegistryCapture, McpRegistryController,
    McpRegistryError, McpRegistryPublication, McpRegistryRefresh, McpRegistryRuntime,
    McpRegistryStatus, McpServerDiagnostic, McpServerState, McpServerStatus, RegistryScope,
    RegistryTrigger,
};
pub use run_server::{
    McpRunBindingsBuilder, McpRunExecutionSpawner, McpRunResourceReader, McpRunResourceRequest,
    McpRunRootResolver, McpRunSelectionPolicy, McpRunServer, McpRunServerConfig,
    McpRunServerFactory, McpRunServerPorts, SanitizedMcpRunResource, serialize_run_snapshot,
};
pub(crate) use runtime::McpRuntimeCandidateAdmission;
pub use runtime::{MCP_DEFAULT_MAX_SSE_EVENT_SIZE, MCP_DEFAULT_OPERATION_TIMEOUT};
pub use server::{
    McpServerCallOutcome, McpServerPort, McpServerResourceContents, McpServerResourceDescriptor,
    McpServerToolCall, serve_stdio,
};
pub use state::{
    CacheScope, McpInputMapping, McpInputRequestId, McpInputRequired, McpInputResponse,
    McpInputState, McpManualOutcome, McpMappedInput, McpRefreshRequest, McpResultMetadata,
    McpRetryRequest, McpSubscription, McpSubscriptionEvent, McpTaskState, McpToolResult,
};

mod session_stdio;
pub use session_stdio::McpStdioSession;
