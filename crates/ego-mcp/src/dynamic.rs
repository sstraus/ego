//! Admission-owned projection of current dynamic Speakeasy tool families.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use ego_core::generator::types::ToolDescriptor;
use ego_core::permit::{ForeignToolId, ForeignTransportTarget, InvocationDigest, Op, PreparedCall};
use jsonschema::Validator;
use serde_json::{Value, json};

use crate::{McpError, McpFailure};

const EXECUTE_TOOL: &str = "execute_tool";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DynamicKind {
    Standalone,
    Gram,
}

/// A dynamic family proven from an admitted `tools/list` snapshot.
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

    /// Detect exactly one current family from names and server-supplied schemas.
    pub fn detect(tools: &[ToolDescriptor]) -> Result<Option<Self>, McpError> {
        let by_name: BTreeMap<&str, &Value> = tools
            .iter()
            .map(|tool| (tool.name.as_str(), &tool.parameters))
            .collect();
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

    fn name_field(&self) -> &'static str {
        match self.kind {
            DynamicKind::Standalone => "name",
            DynamicKind::Gram => "name",
        }
    }

    fn expected_names(&self) -> &'static [&'static str] {
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

#[derive(Clone)]
struct CompiledInner {
    validator: Validator,
}

/// One dynamic dispatcher with admission-compiled inner schemas.
pub struct McpDynamicTool {
    server: Arc<str>,
    wrapper: Arc<str>,
    family: McpDynamicFamily,
    transport: ForeignTransportTarget,
    wrapper_validator: Validator,
    inner: RwLock<BTreeMap<String, Arc<CompiledInner>>>,
}

impl McpDynamicTool {
    pub fn new(wrapper: &str, family: McpDynamicFamily) -> Result<Self, McpError> {
        Self::for_server("dynamic", wrapper, family)
    }

    pub fn for_server(
        server: &str,
        wrapper: &str,
        family: McpDynamicFamily,
    ) -> Result<Self, McpError> {
        let transport =
            ForeignTransportTarget::stdio(server).map_err(|error| invalid(error.to_string()))?;
        Self::for_transport(server, wrapper, family, transport)
    }

    pub fn for_transport(
        server: &str,
        wrapper: &str,
        family: McpDynamicFamily,
        transport: ForeignTransportTarget,
    ) -> Result<Self, McpError> {
        if wrapper != EXECUTE_TOOL || server.is_empty() {
            return Err(invalid("dynamic MCP tool identity is invalid"));
        }
        let mut properties = serde_json::Map::new();
        properties.insert(
            family.name_field().to_owned(),
            json!({"type": "string", "minLength": 1}),
        );
        properties.insert("arguments".to_owned(), json!({}));
        let schema = json!({
            "type": "object",
            "properties": properties,
            "required": [family.name_field()],
            "additionalProperties": false
        });
        let wrapper_validator = family.compile_wrapper(&schema)?;
        Ok(Self {
            server: Arc::from(server),
            wrapper: Arc::from(wrapper),
            family,
            transport,
            wrapper_validator,
            inner: RwLock::new(BTreeMap::new()),
        })
    }

    pub fn family_names(&self) -> &'static [&'static str] {
        self.family.expected_names()
    }

    /// Compile an inner schema during registry admission, before capture.
    pub fn compile_inner(&self, name: &str, schema: Value) -> Result<(), McpError> {
        if name.trim().is_empty() || name == self.wrapper.as_ref() {
            return Err(invalid("dynamic inner tool identity is invalid"));
        }
        let compiled = Arc::new(CompiledInner {
            validator: compile_schema(&schema, "dynamic inner tool")?,
        });
        let mut inner = self
            .inner
            .write()
            .map_err(|_| protocol("dynamic schema registry lock poisoned"))?;
        if inner.insert(name.to_owned(), compiled).is_some() {
            return Err(protocol("dynamic inner tool was compiled more than once"));
        }
        Ok(())
    }

    /// Freeze the currently admitted inner schemas under one capture identity.
    pub fn capture(&self, capture_id: &str) -> McpDynamicCapture {
        let inner = self
            .inner
            .read()
            .map(|inner| inner.clone())
            .unwrap_or_default();
        McpDynamicCapture {
            server: Arc::clone(&self.server),
            wrapper: Arc::clone(&self.wrapper),
            family: self.family.clone(),
            transport: self.transport.clone(),
            capture_id: Arc::from(capture_id),
            inner: Arc::new(inner),
        }
    }

    pub fn prepare_wrapper(
        &self,
        params: Value,
        capture_id: &str,
    ) -> Result<McpPreparedDynamicCall, McpError> {
        if capture_id.is_empty() {
            return Err(invalid("dynamic admission capture identity is empty"));
        }
        validate(
            &self.wrapper_validator,
            &params,
            "dynamic wrapper parameters",
        )?;
        self.capture(capture_id).prepare(params)
    }
}

/// An immutable admission slice. Refresh creates a different value rather than
/// mutating a generation already observed by a model.
#[derive(Clone)]
pub struct McpDynamicCapture {
    server: Arc<str>,
    wrapper: Arc<str>,
    family: McpDynamicFamily,
    transport: ForeignTransportTarget,
    capture_id: Arc<str>,
    inner: Arc<BTreeMap<String, Arc<CompiledInner>>>,
}

impl McpDynamicCapture {
    pub fn capture_id(&self) -> &str {
        &self.capture_id
    }

    pub fn validate(&self, name: &str, arguments: Value) -> Result<(), McpError> {
        let compiled = self
            .inner
            .get(name)
            .ok_or_else(|| protocol("dynamic inner tool is absent from the captured admission"))?;
        validate(&compiled.validator, &arguments, "dynamic inner arguments")
    }

    pub fn validate_after_refresh(
        &self,
        current_capture_id: &str,
        name: &str,
        arguments: Value,
    ) -> Result<(), McpError> {
        if current_capture_id != self.capture_id.as_ref() {
            return Err(McpError::new(
                McpFailure::Authority,
                "dynamic admission capture is stale",
            ));
        }
        self.validate(name, arguments)
    }

    fn prepare(&self, params: Value) -> Result<McpPreparedDynamicCall, McpError> {
        let object = params
            .as_object()
            .ok_or_else(|| invalid("dynamic wrapper parameters are not an object"))?;
        let name = object
            .get(self.family.name_field())
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| invalid("dynamic wrapper has no inner tool name"))?;
        let arguments = object
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        self.validate(name, arguments.clone())?;
        let outer = ForeignToolId::new(&self.server, &self.wrapper)
            .map_err(|error| invalid(error.to_string()))?;
        let inner =
            ForeignToolId::new(&self.server, name).map_err(|error| invalid(error.to_string()))?;
        let mut call = PreparedCall::foreign_protocol(
            outer,
            digest_json(&params)?,
            self.transport.clone(),
            &self.capture_id,
        );
        call.ops.insert(
            1,
            Op::Opaque {
                tool: inner,
                params_digest: digest_json(&arguments)?,
                transport: self.transport.clone(),
            },
        );
        Ok(McpPreparedDynamicCall {
            capture_id: Arc::clone(&self.capture_id),
            inner_name: Arc::from(name),
            inner_arguments: arguments,
            ops: call.ops,
        })
    }
}

pub struct McpPreparedDynamicCall {
    capture_id: Arc<str>,
    inner_name: Arc<str>,
    inner_arguments: Value,
    pub ops: Vec<Op>,
}

impl McpPreparedDynamicCall {
    pub fn capture_id(&self) -> &str {
        &self.capture_id
    }

    pub fn inner_name(&self) -> &str {
        &self.inner_name
    }

    pub fn inner_arguments(&self) -> &Value {
        &self.inner_arguments
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

fn digest_json(value: &Value) -> Result<InvocationDigest, McpError> {
    Ok(InvocationDigest::from_bytes(&serde_json::to_vec(value)?))
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::InvalidValue, message)
}

fn protocol(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::Protocol, message)
}
