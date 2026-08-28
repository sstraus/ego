use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use ego_core::permit::{DuplexHandle, DuplexSettlement, ToolResult};
use futures::io::{AsyncRead, AsyncWrite};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ServerHandler, ServiceExt};
use serde_json::Map;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::{McpError, McpFailure, McpFuture};

pub struct McpServerToolCall {
    name: String,
    arguments: Map<String, serde_json::Value>,
    request_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerResourceDescriptor {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerResourceContents {
    pub uri: String,
    pub mime_type: String,
    pub text: String,
    pub blob: Option<String>,
}

impl McpServerToolCall {
    pub fn new(
        name: String,
        arguments: Map<String, serde_json::Value>,
        request_state: Option<String>,
    ) -> Result<Self, McpError> {
        if name.is_empty() || request_state.as_ref().is_some_and(String::is_empty) {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "invalid MCP server tool call",
            ));
        }
        Ok(Self {
            name,
            arguments,
            request_state,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn arguments(&self) -> &Map<String, serde_json::Value> {
        &self.arguments
    }

    pub fn request_state(&self) -> Option<&str> {
        self.request_state.as_deref()
    }
}

pub trait McpServerPort: Send + Sync {
    fn list_tools<'a>(&'a self) -> McpFuture<'a, Vec<ego_core::generator::types::ToolDescriptor>>;

    fn call_tool<'a>(
        &'a self,
        call: McpServerToolCall,
    ) -> McpFuture<'a, Result<ToolResult, McpError>>;

    fn list_resources<'a>(&'a self) -> McpFuture<'a, Vec<McpServerResourceDescriptor>>;

    fn read_resource<'a>(
        &'a self,
        uri: &'a str,
    ) -> McpFuture<'a, Result<McpServerResourceContents, McpError>>;
}

#[derive(Clone)]
struct ServerAdapter {
    name: Arc<str>,
    port: Arc<dyn McpServerPort>,
}

impl ServerHandler for ServerAdapter {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Owned(vec![ProtocolVersion::V_2026_07_28])
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2026_07_28)
        .with_server_info(Implementation::new(
            self.name.to_string(),
            env!("CARGO_PKG_VERSION"),
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let descriptors = self.port.list_tools().await;
        let mut tools = Vec::with_capacity(descriptors.len());
        for descriptor in descriptors {
            let input_schema = descriptor.parameters.as_object().cloned().ok_or_else(|| {
                rmcp::ErrorData::invalid_params("tool input schema must be an object", None)
            })?;
            tools.push(Tool::new(
                descriptor.name,
                descriptor.description,
                input_schema,
            ));
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let call = McpServerToolCall::new(
            request.name.into_owned(),
            request.arguments.unwrap_or_default(),
            request.request_state,
        )
        .map_err(|error| rmcp::ErrorData::invalid_params(error.to_string(), None))?;
        let result = self
            .port
            .call_tool(call)
            .await
            .map_err(|error| rmcp::ErrorData::invalid_params(error.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(result.model_text())]).into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::ErrorData> {
        let resources = self
            .port
            .list_resources()
            .await
            .into_iter()
            .map(|descriptor| {
                let mut resource = Resource::new(descriptor.uri, descriptor.name)
                    .with_description(descriptor.description);
                if let Some(mime_type) = descriptor.mime_type {
                    resource = resource.with_mime_type(mime_type);
                }
                resource
            })
            .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, rmcp::ErrorData> {
        let contents = self
            .port
            .read_resource(&request.uri)
            .await
            .map_err(|error| match error.failure() {
                McpFailure::Absent => rmcp::ErrorData::resource_not_found(error.to_string(), None),
                _ => rmcp::ErrorData::invalid_params(error.to_string(), None),
            })?;
        let resource = match contents.blob {
            Some(blob) => {
                ResourceContents::blob(blob, contents.uri).with_mime_type(contents.mime_type)
            }
            None => ResourceContents::text(contents.text, contents.uri)
                .with_mime_type(contents.mime_type),
        };
        Ok(ReadResourceResult::new(vec![resource]).into())
    }
}

struct SharedDuplex {
    inner: Arc<Mutex<Option<DuplexHandle>>>,
}

impl AsyncRead for SharedDuplex {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut guard = self.inner.lock().expect("duplex lock");
        match guard.as_mut() {
            Some(duplex) => std::pin::Pin::new(duplex.as_mut()).poll_read(cx, buffer),
            None => Poll::Ready(Ok(0)),
        }
    }
}

impl AsyncWrite for SharedDuplex {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut guard = self.inner.lock().expect("duplex lock");
        match guard.as_mut() {
            Some(duplex) => std::pin::Pin::new(duplex.as_mut()).poll_write(cx, buffer),
            None => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut guard = self.inner.lock().expect("duplex lock");
        match guard.as_mut() {
            Some(duplex) => std::pin::Pin::new(duplex.as_mut()).poll_flush(cx),
            None => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut guard = self.inner.lock().expect("duplex lock");
        match guard.as_mut() {
            Some(duplex) => std::pin::Pin::new(duplex.as_mut()).poll_close(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

pub async fn serve_stdio(
    port: Arc<dyn McpServerPort>,
    duplex: DuplexHandle,
) -> Result<DuplexSettlement, McpError> {
    let inner = Arc::new(Mutex::new(Some(duplex)));
    let transport = SharedDuplex {
        inner: Arc::clone(&inner),
    }
    .compat();
    let adapter = ServerAdapter {
        name: Arc::from("ego"),
        port,
    };
    if let Ok(service) = adapter.serve(transport).await {
        let _ = service.waiting().await;
    }
    let duplex = inner
        .lock()
        .map_err(|_| McpError::new(McpFailure::Transport, "duplex lock poisoned"))?
        .take()
        .ok_or_else(|| McpError::new(McpFailure::Transport, "duplex settlement was lost"))?;
    Ok(crate::stdio::settle(duplex).await)
}
