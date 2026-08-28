//! Immutable MCP operation definitions captured by one registry generation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ego_core::config::McpServerConfig;
use ego_core::generator::types::ToolDescriptor as GeneratorToolDescriptor;
use ego_core::mcp::{McpAdmissionFingerprint, McpOperationKind, McpSubject, ToolExecution};
use ego_core::permit::{
    ForeignToolId, GuardedIo, InvocationDigest, PrepareCtx, PreparedCall, Tool, ToolDescriptor,
    ToolError,
};
use futures::future::BoxFuture;
use jsonschema::Validator;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{McpError, McpFailure};

const EXECUTE_TOOL: &str = "execute_tool";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DynamicKind {
    Standalone,
    Gram,
}

/// A current dynamic family proven from its management-tool schemas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpDynamicFamily {
    kind: DynamicKind,
}

impl McpDynamicFamily {
    pub fn standalone() -> Self {
        Self {
            kind: DynamicKind::Standalone,
        }
    }

    pub fn gram() -> Self {
        Self {
            kind: DynamicKind::Gram,
        }
    }

    pub fn detect(tools: &[GeneratorToolDescriptor]) -> Result<Option<Self>, McpError> {
        let by_name: BTreeMap<&str, &Value> = tools
            .iter()
            .map(|tool| (tool.name.as_str(), &tool.parameters))
            .collect();
        if by_name.len() != tools.len() {
            return Err(protocol("duplicate MCP tool identity"));
        }
        let names: BTreeSet<&str> = by_name.keys().copied().collect();
        let standalone = ["list_tools", "describe_tool", EXECUTE_TOOL]
            .iter()
            .all(|name| names.contains(name));
        let gram = ["search_tools", "describe_tools", EXECUTE_TOOL]
            .iter()
            .all(|name| names.contains(name));
        if standalone == gram {
            return if standalone {
                Err(protocol("ambiguous dynamic MCP family"))
            } else {
                Ok(None)
            };
        }
        let family = if standalone {
            Self::standalone()
        } else {
            Self::gram()
        };
        let execute_schema = by_name
            .get(EXECUTE_TOOL)
            .ok_or_else(|| protocol("dynamic family has no execute_tool schema"))?;
        family.compile_wrapper(execute_schema)?;
        Ok(Some(family))
    }

    pub(crate) fn name_field(&self) -> &'static str {
        "name"
    }

    pub(crate) fn management_names(&self) -> &'static [&'static str] {
        match self.kind {
            DynamicKind::Standalone => {
                &["list_tools", "describe_tool", EXECUTE_TOOL, "list_scopes"]
            }
            DynamicKind::Gram => &["search_tools", "describe_tools", EXECUTE_TOOL],
        }
    }

    fn compile_wrapper(&self, schema: &Value) -> Result<Validator, McpError> {
        let validator = compile_schema(schema, "dynamic wrapper")?;
        let mut valid_probe = serde_json::Map::new();
        valid_probe.insert(self.name_field().to_owned(), json!("probe"));
        valid_probe.insert("arguments".to_owned(), json!({}));
        let valid_probe = Value::Object(valid_probe);
        let mut unknown_probe = valid_probe.clone();
        unknown_probe
            .as_object_mut()
            .expect("probe is an object")
            .insert("unexpected".to_owned(), Value::Bool(true));
        if !validator.is_valid(&valid_probe)
            || validator.is_valid(&json!({"arguments": {}}))
            || validator.is_valid(&unknown_probe)
        {
            return Err(protocol(
                "execute_tool does not expose the current dynamic wrapper schema",
            ));
        }
        Ok(validator)
    }
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
pub struct McpDynamicCatalogue {
    family: McpDynamicFamily,
    inner: Arc<BTreeMap<String, Arc<CompiledSchema>>>,
}

impl McpDynamicCatalogue {
    pub fn new(
        family: McpDynamicFamily,
        tools: Vec<GeneratorToolDescriptor>,
    ) -> Result<Self, McpError> {
        if tools.is_empty() {
            return Err(protocol("dynamic MCP catalogue is empty"));
        }
        let management = family
            .management_names()
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let mut inner = BTreeMap::new();
        for tool in tools {
            if tool.name.trim().is_empty() || management.contains(tool.name.as_str()) {
                return Err(invalid("dynamic inner tool identity is invalid"));
            }
            let compiled = Arc::new(CompiledSchema::new(tool.parameters, "dynamic inner tool")?);
            if inner.insert(tool.name, compiled).is_some() {
                return Err(protocol("duplicate dynamic inner tool identity"));
            }
        }
        Ok(Self {
            family,
            inner: Arc::new(inner),
        })
    }

    pub fn family(&self) -> &McpDynamicFamily {
        &self.family
    }

    pub fn tool_names(&self) -> impl Iterator<Item = &str> {
        self.inner.keys().map(String::as_str)
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
        config: &McpServerConfig,
        operation: McpOperation,
    ) -> Result<PreparedCall, ToolError>;

    fn execute<'a>(
        &'a self,
        config: &'a McpServerConfig,
        call: PreparedCall,
        io: &'a dyn GuardedIo,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>;
}

pub(crate) struct McpToolDefinition {
    descriptor: GeneratorToolDescriptor,
    outer: Arc<CompiledSchema>,
    dynamic: Option<McpDynamicCatalogue>,
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
            dynamic: None,
        })
    }

    pub(crate) fn dynamic(
        descriptor: GeneratorToolDescriptor,
        catalogue: McpDynamicCatalogue,
    ) -> Result<Self, McpError> {
        if descriptor.name != EXECUTE_TOOL {
            return Err(invalid("dynamic wrapper identity is invalid"));
        }
        catalogue.family.compile_wrapper(&descriptor.parameters)?;
        let outer = Arc::new(CompiledSchema::new(
            descriptor.parameters.clone(),
            "dynamic wrapper",
        )?);
        Ok(Self {
            descriptor,
            outer,
            dynamic: Some(catalogue),
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.descriptor.name
    }

    fn admission_schema_fingerprint(&self) -> String {
        let Some(dynamic) = &self.dynamic else {
            return self.outer.fingerprint.to_string();
        };
        let mut hash = Sha256::new();
        hash.update(self.outer.fingerprint.as_bytes());
        for (name, schema) in dynamic.inner.iter() {
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
        config: Arc<McpServerConfig>,
        factory: Arc<dyn McpOperationFactory>,
    ) -> Result<Arc<dyn Tool>, McpError> {
        let admission = McpAdmissionFingerprint::new(
            generation,
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
            config,
            factory,
            admission,
            outer: Arc::clone(&self.outer),
            dynamic: self.dynamic.clone(),
        }))
    }
}

/// The concrete immutable tool retained by a registry capture.
pub struct McpCapturedTool {
    descriptor: ToolDescriptor,
    server: Arc<str>,
    config: Arc<McpServerConfig>,
    factory: Arc<dyn McpOperationFactory>,
    admission: McpAdmissionFingerprint,
    outer: Arc<CompiledSchema>,
    dynamic: Option<McpDynamicCatalogue>,
}

impl McpCapturedTool {
    fn operation(&self, params: &Value) -> Result<McpOperation, ToolError> {
        validate(&self.outer.validator, params, "MCP tool parameters").map_err(tool_params)?;
        let outer = ForeignToolId::new(&self.server, &self.descriptor.name)
            .map_err(|error| ToolError::InvalidParams(error.to_string()))?;
        let mut invocations = vec![(outer, digest_json(params).map_err(tool_params)?)];
        if let Some(dynamic) = &self.dynamic {
            let object = params.as_object().ok_or_else(|| {
                ToolError::InvalidParams("dynamic wrapper parameters are not an object".to_owned())
            })?;
            let name = object
                .get(dynamic.family.name_field())
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| {
                    ToolError::InvalidParams("dynamic wrapper has no inner tool name".to_owned())
                })?;
            let arguments = object
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let inner = dynamic.inner.get(name).ok_or_else(|| {
                ToolError::InvalidParams(
                    "dynamic inner tool is absent from the captured admission".to_owned(),
                )
            })?;
            validate(&inner.validator, &arguments, "dynamic inner arguments")
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
        self.factory.prepare(&self.config, operation)
    }

    fn execute<'a>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>> {
        self.factory.execute(&self.config, call, io)
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

fn schema_fingerprint(schema: &Value) -> Result<String, McpError> {
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
