//! OS-owned materialization and guarded effects for the MCP runtime.

pub use crate::mcp_oauth::{LoopbackOAuthInteraction, oauth_session, persist_oauth_locator};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ego_core::config::{McpConfigProvenance, McpHttpAuth, McpServerConfig, McpServerName};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{DuplexSpawner, GuardedHttp, SpawnSpec};
use ego_mcp::{
    McpDuplexEffects, McpDuplexEffectsFactory, McpEnvironmentBinding, McpError, McpFailure,
    McpFuture, McpHttpEffects, McpHttpEnd, McpHttpResponse, McpHttpSettlement,
    McpRegistryCandidate, McpRequestId, McpServerAdmission, McpTransportMaterial,
    PreparedMcpHttpCall,
};

use crate::exec::RunEffectPorts;

/// Materialize exactly the environment names admitted by resolved MCP
/// configuration. No ambient value outside `inherit_env` crosses into the
/// live, non-serializable admission authority. A server whose live material
/// cannot be retained becomes one sanitized rejected registry candidate;
/// independent server materialization continues.
pub fn materialize_admissions(
    selected: Vec<(McpServerName, McpServerConfig, McpConfigProvenance)>,
) -> Vec<McpRegistryCandidate> {
    let home = std::env::var_os("EGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".ego"))
        });
    materialize_admissions_with_credentials(
        selected,
        home.as_deref().map(|home| home.join("mcp")).as_deref(),
        Arc::new(crate::credentials::OsCredentialStore::default()),
    )
}

/// Injected store/root seam for restart and credential isolation tests.
pub fn materialize_admissions_with_credentials(
    selected: Vec<(McpServerName, McpServerConfig, McpConfigProvenance)>,
    root: Option<&std::path::Path>,
    credentials: Arc<dyn ego_core::credentials::CredentialStore>,
) -> Vec<McpRegistryCandidate> {
    selected
        .into_iter()
        .map(|(name, server, provenance)| {
            let material = match &server {
                McpServerConfig::Stdio(stdio) => {
                    let environment = stdio
                        .inherit_env
                        .iter()
                        .map(|variable| {
                            let value = std::env::var_os(variable.as_str())
                                .ok_or(McpFailure::Credential)?;
                            McpEnvironmentBinding::from_native(variable.clone(), value)
                                .map_err(|error| error.failure())
                        })
                        .collect::<Result<Vec<_>, McpFailure>>();
                    environment.map(McpTransportMaterial::stdio)
                }
                McpServerConfig::Http(http) if http.auth == McpHttpAuth::Oauth => root
                    .ok_or(McpFailure::Credential)
                    .and_then(|root| {
                        oauth_session(root, &name, http, Arc::clone(&credentials))
                            .map_err(|error| error.failure())
                    })
                    .map(McpTransportMaterial::oauth),
                McpServerConfig::Http(_) => Ok(McpTransportMaterial::http(Vec::new())),
                McpServerConfig::Acp { .. } => Err(McpFailure::Authority),
            };
            match material.and_then(|material| {
                McpServerAdmission::new(name.clone(), server.clone(), material)
                    .map_err(|error| error.failure())
            }) {
                Ok(admission) => McpRegistryCandidate::ready(Arc::new(admission), provenance),
                Err(failure) => McpRegistryCandidate::rejected(name, server, provenance, failure),
            }
        })
        .collect()
}

/// Current-protocol MCP management HTTP over one command-lifetime guarded
/// effect scope. Request identity and settlement remain in this adapter; the
/// OS transport owns only DNS, the selected connection, and response bytes.
pub struct RunMcpHttpEffects {
    ports: Arc<RunEffectPorts>,
    requests: Arc<Mutex<HashMap<McpRequestId, RequestState>>>,
}

struct RequestState {
    delivery: DeliveryCertainty,
    settlement: Option<McpHttpSettlement>,
}

impl RunMcpHttpEffects {
    pub fn new(ports: Arc<RunEffectPorts>) -> Self {
        Self {
            ports,
            requests: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn settle_request(
        requests: &Mutex<HashMap<McpRequestId, RequestState>>,
        request_id: McpRequestId,
        end: McpHttpEnd,
    ) -> Result<McpHttpSettlement, McpError> {
        let mut requests = requests
            .lock()
            .map_err(|_| transport_error("MCP HTTP request state lock poisoned"))?;
        let state = requests
            .remove(&request_id)
            .ok_or_else(|| authority_error("MCP HTTP request identity is not active"))?;
        if let Some(settlement) = state.settlement {
            return Ok(settlement);
        }
        let settlement = McpHttpSettlement::new(request_id, end, state.delivery)?;
        Ok(settlement)
    }
}

impl McpHttpEffects for RunMcpHttpEffects {
    fn execute(
        &self,
        call: PreparedMcpHttpCall,
    ) -> McpFuture<'static, Result<McpHttpResponse, McpError>> {
        let parts = call.into_parts();
        let request_id = parts.request_id.clone();
        let inserted = self.requests.lock().map(|mut requests| {
            requests
                .insert(
                    request_id.clone(),
                    RequestState {
                        // Once the admitted request future is handed to the
                        // transport, cancellation must conservatively retain
                        // possibly-sent delivery unless the transport returns
                        // a more precise failure.
                        delivery: DeliveryCertainty::PossiblySent,
                        settlement: None,
                    },
                )
                .is_none()
        });
        match inserted {
            Ok(true) => {}
            Ok(false) => {
                return Box::pin(futures::future::ready(Err(authority_error(
                    "MCP HTTP request identity was reused",
                ))));
            }
            Err(_) => {
                return Box::pin(futures::future::ready(Err(transport_error(
                    "MCP HTTP request state lock poisoned",
                ))));
            }
        }

        let ports = Arc::clone(&self.ports);
        let requests = Arc::clone(&self.requests);
        Box::pin(async move {
            let outcome = GuardedHttp::new(ports.resolver_handle(), ports.one_hop_handle())
                .execute_admitted(parts.route, parts.connect, parts.request, parts.deadline)
                .await;
            match outcome {
                Ok(response) => {
                    let delivery = response.delivery();
                    if let Ok(mut states) = requests.lock()
                        && let Some(state) = states.get_mut(&request_id)
                    {
                        state.delivery = delivery;
                    }
                    Ok(McpHttpResponse::from_one_hop(response))
                }
                Err(error) => {
                    let delivery = error.delivery();
                    if let Ok(mut states) = requests.lock()
                        && let Some(state) = states.get_mut(&request_id)
                    {
                        state.delivery = delivery;
                        state.settlement =
                            McpHttpSettlement::new(request_id, McpHttpEnd::Failed, delivery).ok();
                    }
                    Err(McpError::from(error))
                }
            }
        })
    }

    fn cancel(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        let result = Self::settle_request(&self.requests, request_id, McpHttpEnd::Cancelled);
        Box::pin(futures::future::ready(result))
    }

    fn timeout(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        let result = Self::settle_request(&self.requests, request_id, McpHttpEnd::TimedOut);
        Box::pin(futures::future::ready(result))
    }

    fn settle(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        let result = Self::settle_request(&self.requests, request_id, McpHttpEnd::Completed);
        Box::pin(futures::future::ready(result))
    }
}

/// Factory for one fresh stdio discovery lifecycle. The live admission owns
/// the exact environment material; this adapter owns only OS spawning.
pub struct RunMcpDuplexEffectsFactory {
    spawner: Arc<dyn DuplexSpawner>,
}

impl RunMcpDuplexEffectsFactory {
    pub fn new(spawner: Arc<dyn DuplexSpawner>) -> Self {
        Self { spawner }
    }
}

impl McpDuplexEffectsFactory for RunMcpDuplexEffectsFactory {
    fn create(
        &self,
        server: McpServerName,
        spec: SpawnSpec,
    ) -> Result<Box<dyn McpDuplexEffects>, McpError> {
        Ok(Box::new(FreshMcpDuplex {
            server,
            spec: Some(spec),
            spawner: Arc::clone(&self.spawner),
        }))
    }
}

struct FreshMcpDuplex {
    server: McpServerName,
    spec: Option<SpawnSpec>,
    spawner: Arc<dyn DuplexSpawner>,
}

impl McpDuplexEffects for FreshMcpDuplex {
    fn open(
        mut self: Box<Self>,
        server: McpServerName,
    ) -> McpFuture<'static, Result<ego_core::permit::DuplexHandle, McpError>> {
        if server != self.server {
            return Box::pin(futures::future::ready(Err(authority_error(
                "MCP stdio server identity changed",
            ))));
        }
        let Some(spec) = self.spec.take() else {
            return Box::pin(futures::future::ready(Err(authority_error(
                "MCP stdio authority was already spent",
            ))));
        };
        let spawner = Arc::clone(&self.spawner);
        Box::pin(async move { spawner.spawn(spec).await.map_err(McpError::from) })
    }
}

fn authority_error(message: &'static str) -> McpError {
    McpError::new(McpFailure::Authority, message)
}

fn transport_error(message: &'static str) -> McpError {
    McpError::new(McpFailure::Transport, message)
}
