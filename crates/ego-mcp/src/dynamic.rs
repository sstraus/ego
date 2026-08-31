//! Immutable MCP operation definitions captured by one registry generation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ego_core::generator::types::ToolDescriptor as GeneratorToolDescriptor;
use ego_core::hook::EffectScope;
use ego_core::mcp::{McpAdmissionFingerprint, McpOperationKind, McpSubject, ToolExecution};
use ego_core::permit::{
    ForeignToolId, GuardedIo, InvocationDigest, PrepareCtx, PreparedCall, Tool, ToolDescriptor,
    ToolError,
};
use futures::future::BoxFuture;
use jsonschema::Validator;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{McpError, McpFailure, McpServerAdmission};

const SEARCH_TOOLS: &str = "search_tools";
const GET_TOOL_SCHEMA: &str = "get_tool_schema";
const CALL_TOOL: &str = "call_tool";
const COLLAPSED_MANAGEMENT_TOOLS: [&str; 3] = [SEARCH_TOOLS, GET_TOOL_SCHEMA, CALL_TOOL];

/// The one collapsed MCP bootstrap proven from compatible management schemas.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct McpCollapsedBootstrap;

impl McpCollapsedBootstrap {
    pub fn new(tools: Vec<GeneratorToolDescriptor>) -> Result<Self, McpError> {
        Self::detect(&tools)?
            .ok_or_else(|| protocol("MCP tools do not expose the collapsed bootstrap contract"))
    }

    pub fn detect(tools: &[GeneratorToolDescriptor]) -> Result<Option<Self>, McpError> {
        let by_name: BTreeMap<&str, &Value> = tools
            .iter()
            .map(|tool| (tool.name.as_str(), &tool.parameters))
            .collect();
        if by_name.len() != tools.len() {
            return Err(protocol("duplicate MCP tool identity"));
        }
        let Some(search_schema) = by_name.get(SEARCH_TOOLS) else {
            return Ok(None);
        };
        let Some(lookup_schema) = by_name.get(GET_TOOL_SCHEMA) else {
            return Ok(None);
        };
        let Some(dispatch_schema) = by_name.get(CALL_TOOL) else {
            return Ok(None);
        };
        if compatible_search_schema(search_schema)
            && compatible_lookup_schema(lookup_schema)
            && compatible_dispatch_schema(dispatch_schema)
        {
            Ok(Some(Self))
        } else {
            Ok(None)
        }
    }

    pub fn tool_names(&self) -> impl Iterator<Item = &'static str> {
        [GET_TOOL_SCHEMA, SEARCH_TOOLS].into_iter()
    }

    pub(crate) fn lookup_name(&self) -> &'static str {
        GET_TOOL_SCHEMA
    }

    pub(crate) fn dispatch_name(&self) -> &'static str {
        CALL_TOOL
    }

    pub(crate) fn requested_name(&self, parameters: &Value) -> Result<String, McpError> {
        let object = parameters
            .as_object()
            .ok_or_else(|| invalid("collapsed schema lookup parameters are not an object"))?;
        object
            .get("tool_name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| invalid("collapsed schema lookup has no requested tool name"))
    }

    fn compile_dispatch(&self, schema: &Value) -> Result<Validator, McpError> {
        if !compatible_dispatch_schema(schema) {
            return Err(protocol(
                "call_tool does not expose the collapsed dispatch schema",
            ));
        }
        compile_schema(schema, "collapsed dispatch")
    }
}

fn compatible_search_schema(schema: &Value) -> bool {
    jsonschema::validator_for(schema).is_ok_and(|validator| {
        validator.is_valid(&json!({"query": "probe"}))
            && validator.is_valid(&json!({"query": "probe", "limit": 1}))
            && !validator.is_valid(&json!({}))
            && !validator.is_valid(&json!({"query": 1}))
            && !validator.is_valid(&json!({"query": "probe", "limit": "1"}))
    })
}

fn compatible_lookup_schema(schema: &Value) -> bool {
    jsonschema::validator_for(schema).is_ok_and(|validator| {
        validator.is_valid(&json!({"tool_name": "probe"}))
            && !validator.is_valid(&json!({}))
            && !validator.is_valid(&json!({"tool_name": 1}))
            && !validator.is_valid(&json!({"name": "probe"}))
    })
}

fn compatible_dispatch_schema(schema: &Value) -> bool {
    jsonschema::validator_for(schema).is_ok_and(|validator| {
        validator.is_valid(&json!({"tool_name": "probe", "arguments": {}}))
            && !validator.is_valid(&json!({"arguments": {}}))
            && !validator.is_valid(&json!({"tool_name": "probe"}))
            && !validator.is_valid(&json!({"tool_name": "probe", "arguments": []}))
            && !validator.is_valid(&json!({"name": "probe", "arguments": {}}))
    })
}

struct CompiledSchema {
    schema: Value,
    validator: Validator,
    fingerprint: Arc<str>,
}

impl CompiledSchema {
    fn new(schema: Value, subject: &str) -> Result<Self, McpError> {
        let validator = compile_schema(&schema, subject)?;
        let fingerprint = schema_fingerprint(&schema)?;
        Ok(Self {
            schema,
            validator,
            fingerprint: Arc::from(fingerprint),
        })
    }
}

/// The inner catalogue discovered and validated before registry publication.
#[derive(Clone)]
pub struct McpCollapsedCatalogue {
    inner: Arc<BTreeMap<String, Arc<CompiledSchema>>>,
}

impl McpCollapsedCatalogue {
    pub fn new(tools: Vec<GeneratorToolDescriptor>) -> Result<Self, McpError> {
        if tools.is_empty() {
            return Err(protocol("collapsed MCP catalogue is empty"));
        }
        let management = COLLAPSED_MANAGEMENT_TOOLS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let mut inner = BTreeMap::new();
        for tool in tools {
            if tool.name.trim().is_empty() || management.contains(tool.name.as_str()) {
                return Err(invalid("collapsed inner tool identity is invalid"));
            }
            if !tool.parameters.is_object() {
                return Err(invalid("collapsed inner tool definition is incomplete"));
            }
            let compiled = Arc::new(CompiledSchema::new(
                tool.parameters,
                "collapsed inner tool",
            )?);
            if inner.insert(tool.name, compiled).is_some() {
                return Err(protocol("duplicate collapsed inner tool identity"));
            }
        }
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    pub fn tool_names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(CALL_TOOL).chain(self.inner.keys().map(String::as_str))
    }

    pub fn from_schema_response(
        _bootstrap: McpCollapsedBootstrap,
        requested: &str,
        result: Value,
    ) -> Result<Self, McpError> {
        let response: CollapsedSchemaResponse = serde_json::from_value(result)
            .map_err(|error| invalid(format!("invalid collapsed schema response: {error}")))?;
        Self::reconcile_definition(None, requested, response.structured_content)
    }

    pub(crate) fn reconcile_structured_content(
        previous: Option<&Self>,
        requested: &str,
        structured_content: Value,
    ) -> Result<Self, McpError> {
        let definition: CollapsedSchemaDefinition = serde_json::from_value(structured_content)
            .map_err(|error| invalid(format!("invalid collapsed tool schema: {error}")))?;
        Self::reconcile_definition(previous, requested, definition)
    }

    fn reconcile_definition(
        previous: Option<&Self>,
        requested: &str,
        definition: CollapsedSchemaDefinition,
    ) -> Result<Self, McpError> {
        Self::reconcile(previous, requested, definition.into_descriptor())
    }

    pub(crate) fn reconcile(
        previous: Option<&Self>,
        requested: &str,
        tool: GeneratorToolDescriptor,
    ) -> Result<Self, McpError> {
        let management = COLLAPSED_MANAGEMENT_TOOLS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if requested.trim().is_empty()
            || management.contains(requested)
            || tool.name != requested
            || !tool.parameters.is_object()
        {
            return Err(invalid(
                "collapsed schema definition does not match its request",
            ));
        }
        let mut inner = previous
            .map(|catalogue| catalogue.inner.as_ref().clone())
            .unwrap_or_default();
        let compiled = Arc::new(CompiledSchema::new(
            tool.parameters,
            "collapsed inner tool",
        )?);
        inner.insert(tool.name, compiled);
        if inner.is_empty() {
            return Err(protocol("collapsed MCP catalogue is empty"));
        }
        Ok(Self {
            inner: Arc::new(inner),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CollapsedSchemaResponse {
    structured_content: CollapsedSchemaDefinition,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CollapsedSchemaDefinition {
    name: String,
    description: String,
    input_schema: Value,
}

impl CollapsedSchemaDefinition {
    fn into_descriptor(self) -> GeneratorToolDescriptor {
        GeneratorToolDescriptor {
            name: self.name,
            description: self.description,
            parameters: self.input_schema,
        }
    }
}

/// One semantic operation, built by a captured tool after schema validation.
pub struct McpOperation {
    subject: McpSubject,
    operation: McpOperationKind,
    admission: McpAdmissionFingerprint,
    parameters: Value,
    invocations: Vec<(ForeignToolId, InvocationDigest)>,
}

impl McpOperation {
    pub(crate) fn captured(
        subject: McpSubject,
        operation: McpOperationKind,
        admission: McpAdmissionFingerprint,
        parameters: Value,
        invocations: Vec<(ForeignToolId, InvocationDigest)>,
    ) -> Self {
        Self {
            subject,
            operation,
            admission,
            parameters,
            invocations,
        }
    }

    pub fn subject(&self) -> &McpSubject {
        &self.subject
    }

    pub fn operation(&self) -> McpOperationKind {
        self.operation
    }

    pub fn admission(&self) -> &McpAdmissionFingerprint {
        &self.admission
    }

    pub fn parameters(&self) -> &Value {
        &self.parameters
    }

    pub fn invocations(&self) -> &[(ForeignToolId, InvocationDigest)] {
        &self.invocations
    }
}

/// Consumer-owned factory for the fresh lifecycle of one selected operation.
pub trait McpOperationFactory: Send + Sync {
    fn prepare(
        &self,
        admission: Arc<McpServerAdmission>,
        operation: McpOperation,
    ) -> Result<PreparedCall, ToolError>;

    fn execute<'a>(
        &'a self,
        admission: Arc<McpServerAdmission>,
        call: PreparedCall,
        io: &'a dyn GuardedIo,
        generation: Arc<str>,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>;

    fn execute_hook<'a>(
        &'a self,
        _admission: Arc<McpServerAdmission>,
        _call: PreparedCall,
        _io: &'a dyn GuardedIo,
        _scope: EffectScope,
    ) -> BoxFuture<'a, Result<crate::McpHookExecution, ToolError>> {
        Box::pin(async {
            Err(ToolError::InvalidParams(
                "MCP operation owner does not expose raw hook structuredContent".to_owned(),
            ))
        })
    }
}

pub(crate) struct McpToolDefinition {
    descriptor: GeneratorToolDescriptor,
    outer: Arc<CompiledSchema>,
    collapsed: Option<McpCollapsedCatalogue>,
}

impl McpToolDefinition {
    pub(crate) fn direct(descriptor: GeneratorToolDescriptor) -> Result<Self, McpError> {
        if descriptor.name.trim().is_empty() {
            return Err(invalid("MCP tool identity is invalid"));
        }
        let outer = Arc::new(CompiledSchema::new(
            descriptor.parameters.clone(),
            "MCP tool",
        )?);
        Ok(Self {
            descriptor,
            outer,
            collapsed: None,
        })
    }

    pub(crate) fn collapsed(
        descriptor: GeneratorToolDescriptor,
        catalogue: McpCollapsedCatalogue,
    ) -> Result<Self, McpError> {
        if descriptor.name != CALL_TOOL {
            return Err(invalid("collapsed dispatch identity is invalid"));
        }
        McpCollapsedBootstrap.compile_dispatch(&descriptor.parameters)?;
        let outer = Arc::new(CompiledSchema::new(
            descriptor.parameters.clone(),
            "collapsed dispatch",
        )?);
        Ok(Self {
            descriptor,
            outer,
            collapsed: Some(catalogue),
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.descriptor.name
    }

    pub(crate) fn collapsed_catalogue(&self) -> Option<&McpCollapsedCatalogue> {
        self.collapsed.as_ref()
    }

    pub(crate) fn admission_schema_fingerprint(&self) -> String {
        let Some(collapsed) = &self.collapsed else {
            return self.outer.fingerprint.to_string();
        };
        let mut hash = Sha256::new();
        hash.update(self.outer.fingerprint.as_bytes());
        for (name, schema) in collapsed.inner.iter() {
            hash.update([0]);
            hash.update(name.as_bytes());
            hash.update([0]);
            hash.update(schema.fingerprint.as_bytes());
        }
        format!("sha256:{:x}", hash.finalize())
    }

    pub(crate) fn capture(
        &self,
        server: &str,
        generation: &str,
        capture: &str,
        server_admission: Arc<McpServerAdmission>,
        factory: Arc<dyn McpOperationFactory>,
    ) -> Result<Arc<McpCapturedTool>, McpError> {
        let admission = McpAdmissionFingerprint::new(
            capture,
            format!("{server}/{}", self.descriptor.name),
            self.admission_schema_fingerprint(),
        )
        .map_err(|error| invalid(error.to_string()))?;
        let schema = serde_json::from_value(self.outer.schema.clone())
            .map_err(|error| invalid(format!("invalid MCP tool schema: {error}")))?;
        Ok(Arc::new(McpCapturedTool {
            descriptor: ToolDescriptor {
                name: self.descriptor.name.clone(),
                description: self.descriptor.description.clone(),
                schema,
            },
            server: Arc::from(server),
            generation: Arc::from(generation),
            server_admission,
            factory,
            admission,
            outer: Arc::clone(&self.outer),
            collapsed: self.collapsed.clone(),
        }))
    }
}

/// The concrete immutable tool retained by a registry capture.
pub struct McpCapturedTool {
    descriptor: ToolDescriptor,
    server: Arc<str>,
    generation: Arc<str>,
    server_admission: Arc<McpServerAdmission>,
    factory: Arc<dyn McpOperationFactory>,
    admission: McpAdmissionFingerprint,
    outer: Arc<CompiledSchema>,
    collapsed: Option<McpCollapsedCatalogue>,
}

pub(crate) type PreparedCapturedToolOperation = (
    Arc<McpServerAdmission>,
    Arc<dyn McpOperationFactory>,
    PreparedCall,
);

impl McpCapturedTool {
    pub(crate) fn matches_subject_parts(
        &self,
        server: &str,
        capture: &str,
        operation: &str,
        schema: &str,
    ) -> bool {
        self.server.as_ref() == server
            && self.admission.capture() == capture
            && self.admission.operation() == operation
            && self.admission.schema() == schema
    }

    pub(crate) fn prepare_captured(
        &self,
        params: Value,
    ) -> Result<PreparedCapturedToolOperation, ToolError> {
        let operation = self.operation(&params)?;
        let call = self
            .factory
            .prepare(Arc::clone(&self.server_admission), operation)?;
        Ok((
            Arc::clone(&self.server_admission),
            Arc::clone(&self.factory),
            call,
        ))
    }

    fn operation(&self, params: &Value) -> Result<McpOperation, ToolError> {
        validate(&self.outer.validator, params, "MCP tool parameters").map_err(tool_params)?;
        let outer = ForeignToolId::new(&self.server, &self.descriptor.name)
            .map_err(|error| ToolError::InvalidParams(error.to_string()))?;
        let mut invocations = vec![(outer, digest_json(params).map_err(tool_params)?)];
        if let Some(collapsed) = &self.collapsed {
            let object = params.as_object().ok_or_else(|| {
                ToolError::InvalidParams(
                    "collapsed dispatch parameters are not an object".to_owned(),
                )
            })?;
            let name = object
                .get("tool_name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| {
                    ToolError::InvalidParams("collapsed dispatch has no inner tool name".to_owned())
                })?;
            let arguments = object
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let inner = collapsed.inner.get(name).ok_or_else(|| {
                ToolError::InvalidParams(
                    "collapsed inner tool is absent from the captured admission".to_owned(),
                )
            })?;
            validate(&inner.validator, &arguments, "collapsed inner arguments")
                .map_err(tool_params)?;
            let inner_id = ForeignToolId::new(&self.server, name)
                .map_err(|error| ToolError::InvalidParams(error.to_string()))?;
            invocations.push((inner_id, digest_json(&arguments).map_err(tool_params)?));
        }
        Ok(McpOperation {
            subject: McpSubject::Tool {
                server: self.server.to_string(),
                name: self.descriptor.name.clone(),
            },
            operation: McpOperationKind::CallTool,
            admission: self.admission.clone(),
            parameters: params.clone(),
            invocations,
        })
    }
}

impl Tool for McpCapturedTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    fn prepare(&self, params: Value, _ctx: &PrepareCtx<'_>) -> Result<PreparedCall, ToolError> {
        let operation = self.operation(&params)?;
        self.factory
            .prepare(Arc::clone(&self.server_admission), operation)
    }

    fn execute<'a>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>> {
        self.factory.execute(
            Arc::clone(&self.server_admission),
            call,
            io,
            Arc::clone(&self.generation),
        )
    }
}

fn compile_schema(schema: &Value, subject: &str) -> Result<Validator, McpError> {
    jsonschema::validator_for(schema)
        .map_err(|error| invalid(format!("invalid {subject} schema: {error}")))
}

fn validate(validator: &Validator, value: &Value, subject: &str) -> Result<(), McpError> {
    let reasons = validator
        .iter_errors(value)
        .map(|error| format!("{}: {error}", error.instance_path()))
        .collect::<Vec<_>>();
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(invalid(format!(
            "invalid {subject}: {}",
            reasons.join("; ")
        )))
    }
}

pub(crate) fn schema_fingerprint(schema: &Value) -> Result<String, McpError> {
    let bytes = serde_json::to_vec(schema)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn digest_json(value: &Value) -> Result<InvocationDigest, McpError> {
    Ok(InvocationDigest::from_bytes(&serde_json::to_vec(value)?))
}

fn tool_params(error: McpError) -> ToolError {
    ToolError::InvalidParams(error.to_string())
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::InvalidValue, message)
}

fn protocol(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::Protocol, message)
}
