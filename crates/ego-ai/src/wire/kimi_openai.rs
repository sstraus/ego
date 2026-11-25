//! Kimi's current OpenAI-shaped Chat Completions contract.

use std::collections::BTreeSet;
use std::sync::Arc;

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{GenRequest, ThinkingLevel};
use futures::stream::BoxStream;

use crate::http::{HttpError, HttpRequest, HttpResponse};

use super::provider_cost::ProviderCost;
use super::{RequestRefused, RequestTarget, WireFormat, openai_completions};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KimiOpenAi;

impl WireFormat for KimiOpenAi {
    fn prompt_cache_policy(&self, req: &GenRequest) -> ego_core::usage::PromptCachePolicy {
        super::cache::policy(req, true, true)
    }

    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        if super::request_has_audio(req) {
            return Err(RequestRefused::new(
                "this wire contract does not support audio input",
            ));
        }
        let body = request_body(req)?;
        Ok(target.post_json("chat/completions", body))
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        openai_completions::decode(response, cost)
    }
}

fn effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some("minimal"),
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::XHigh => Some("xhigh"),
        ThinkingLevel::Max => Some("max"),
    }
}

pub fn request_body(req: &GenRequest) -> Result<Vec<u8>, RequestRefused> {
    let bytes = openai_completions::request_body(req)
        .map_err(|error| RequestRefused::new(error.to_string()))?;
    let mut body: serde_json::Value =
        serde_json::from_slice(&bytes).expect("the shared Chat request serialises a JSON object");
    let object = body
        .as_object_mut()
        .expect("the shared Chat request serialises a JSON object");
    if let Some(key) = super::cache::routing_key(req, "kimi") {
        object.insert("prompt_cache_key".to_string(), key.into());
        let ttl = if req
            .cache
            .as_ref()
            .expect("enabled cache metadata")
            .retention
            == ego_core::generator::capabilities::CacheRetention::Long
        {
            "1h"
        } else {
            "5m"
        };
        object.insert(
            "prompt_cache_options".to_string(),
            serde_json::json!({"mode": "implicit", "ttl": ttl}),
        );
    }
    object.remove("reasoning_effort");
    object.remove("parallel_tool_calls");
    object.insert(
        "thinking".to_string(),
        match effort(req.thinking) {
            Some(effort) => serde_json::json!({"type": "enabled", "effort": effort}),
            None => serde_json::json!({"type": "disabled"}),
        },
    );
    if let Some(tools) = object
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    {
        for (tool, source) in tools.iter_mut().zip(&req.tools) {
            if source.name.starts_with('$') {
                *tool = serde_json::json!({
                    "type": "builtin_function",
                    "function": {"name": source.name}
                });
            } else if let Some(parameters) = tool
                .get_mut("function")
                .and_then(|function| function.get_mut("parameters"))
            {
                *parameters = normalized_schema(&source.parameters)?;
            }
        }
    }
    Ok(serde_json::to_vec(&body).expect("a JSON value serialises"))
}

fn normalized_schema(schema: &serde_json::Value) -> Result<serde_json::Value, RequestRefused> {
    if !schema.is_object() {
        return Err(RequestRefused::new(
            "Kimi tool schema root must be a JSON object",
        ));
    }
    let mut visiting = BTreeSet::new();
    let mut normalized = resolve_refs(schema, schema, &mut visiting);
    normalize_schema(&mut normalized)?;
    for bucket in ["$defs", "definitions"] {
        if !contains_definition_ref(&normalized, bucket) {
            normalized
                .as_object_mut()
                .map(|object| object.remove(bucket));
        }
    }
    Ok(normalized)
}

fn resolve_refs(
    node: &serde_json::Value,
    root: &serde_json::Value,
    visiting: &mut BTreeSet<String>,
) -> serde_json::Value {
    match node {
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| resolve_refs(value, root, visiting))
                .collect(),
        ),
        serde_json::Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(serde_json::Value::as_str)
                && let Some(pointer) = reference.strip_prefix('#')
                && !visiting.contains(reference)
                && let Some(target) = root.pointer(pointer)
            {
                visiting.insert(reference.to_string());
                let resolved = resolve_refs(target, root, visiting);
                visiting.remove(reference);
                if let Some(mut resolved) = resolved.as_object().cloned() {
                    for (key, value) in object {
                        if key != "$ref" {
                            resolved.insert(key.clone(), resolve_refs(value, root, visiting));
                        }
                    }
                    return serde_json::Value::Object(resolved);
                }
            }
            serde_json::Value::Object(
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), resolve_refs(value, root, visiting)))
                    .collect(),
            )
        }
        value => value.clone(),
    }
}

fn contains_definition_ref(node: &serde_json::Value, bucket: &str) -> bool {
    match node {
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| contains_definition_ref(value, bucket)),
        serde_json::Value::Object(object) => {
            object
                .get("$ref")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|reference| reference.starts_with(&format!("#/{bucket}/")))
                || object
                    .iter()
                    .filter(|(key, _)| key.as_str() != bucket)
                    .any(|(_, value)| contains_definition_ref(value, bucket))
        }
        _ => false,
    }
}

fn normalize_schema(schema: &mut serde_json::Value) -> Result<(), RequestRefused> {
    let Some(object) = schema.as_object_mut() else {
        return Ok(());
    };
    for key in [
        "properties",
        "$defs",
        "definitions",
        "patternProperties",
        "dependencies",
        "dependentSchemas",
    ] {
        if let Some(children) = object
            .get_mut(key)
            .and_then(serde_json::Value::as_object_mut)
        {
            for child in children.values_mut() {
                normalize_schema(child)?;
            }
        }
    }
    if let Some(items) = object.get_mut("items") {
        match items {
            serde_json::Value::Array(children) => {
                for child in children {
                    normalize_schema(child)?;
                }
            }
            child => normalize_schema(child)?,
        }
    }
    for key in [
        "additionalProperties",
        "additionalItems",
        "contains",
        "contentSchema",
        "not",
        "if",
        "propertyNames",
        "then",
        "else",
        "unevaluatedItems",
        "unevaluatedProperties",
    ] {
        if let Some(child) = object.get_mut(key) {
            normalize_schema(child)?;
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object
            .get_mut(key)
            .and_then(serde_json::Value::as_array_mut)
        {
            for child in children {
                normalize_schema(child)?;
            }
        }
    }
    if object.contains_key("type")
        || object.contains_key("$ref")
        || ["allOf", "anyOf", "oneOf", "not", "if", "then", "else"]
            .iter()
            .any(|key| object.contains_key(*key))
    {
        return Ok(());
    }
    let inferred = if let Some(values) = object.get("enum").and_then(serde_json::Value::as_array) {
        infer_values(values)?
    } else if let Some(value) = object.get("const") {
        infer_values(std::slice::from_ref(value))?
    } else if object.contains_key("properties") || object.contains_key("required") {
        "object".to_string()
    } else if object.contains_key("items") || object.contains_key("prefixItems") {
        "array".to_string()
    } else {
        "string".to_string()
    };
    object.insert("type".to_string(), serde_json::Value::String(inferred));
    Ok(())
}

fn infer_values(values: &[serde_json::Value]) -> Result<String, RequestRefused> {
    let types = values
        .iter()
        .map(|value| match value {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
        })
        .collect::<BTreeSet<_>>();
    if types.len() != 1 {
        return Err(RequestRefused::new(
            "Kimi tool schema enum or const values have mixed JSON types",
        ));
    }
    Ok(types.into_iter().next().unwrap_or("string").to_string())
}
