//! Hook adapters over the run's admitted process and HTTP effect owners.

use std::ffi::OsString;

use ego_core::credentials::SecretMaterial;
use ego_core::event::hook::HookDecision;
use ego_core::hook::{
    HookHandlerConfig, HookHandlerRequestV1, HookHandlerResultV1, HookInvocationRequest,
};
use ego_core::permit::{
    EnvPolicy, GuardedEffects, HttpHeader, HttpRequest, OneHopPayload, OneHopResponse, Op,
    PreparedCall, ProcessError, SpawnSpec, StreamProvenance, ToolOutputProvenance, ToolResult,
};
use ego_core::store::evidence::EvidenceRef;
use futures::StreamExt as _;

pub use ego_core::generator::attempt::DeliveryCertainty;

const MAX_HOOK_WIRE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookAdapterError(String);

impl HookAdapterError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for HookAdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for HookAdapterError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookAdapterSettlement {
    decision: HookDecision,
    feedback: Option<String>,
    failure: Option<String>,
    evidence: Option<EvidenceRef>,
    delivery: DeliveryCertainty,
}

impl HookAdapterSettlement {
    fn success(
        result: HookHandlerResultV1,
        evidence: Option<EvidenceRef>,
        delivery: DeliveryCertainty,
    ) -> Self {
        Self {
            decision: result.decision(),
            feedback: result.feedback().map(str::to_owned),
            failure: None,
            evidence,
            delivery,
        }
    }

    fn failed(message: impl Into<String>, delivery: DeliveryCertainty) -> Self {
        Self {
            decision: HookDecision::Continue,
            feedback: None,
            failure: Some(message.into()),
            evidence: None,
            delivery,
        }
    }

    pub fn decision(&self) -> HookDecision {
        self.decision
    }

    pub fn feedback(&self) -> Option<&str> {
        self.feedback.as_deref()
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    pub fn evidence(&self) -> Option<&EvidenceRef> {
        self.evidence.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookAdapterResult {
    Settled(HookAdapterSettlement),
}

impl HookAdapterResult {
    pub fn delivery_certainty(&self) -> DeliveryCertainty {
        match self {
            Self::Settled(settlement) => settlement.delivery,
        }
    }
}

pub struct CommandHookAdapter<'a> {
    effects: GuardedEffects<'a>,
    prepared: PreparedCall,
}

impl<'a> CommandHookAdapter<'a> {
    pub fn new(effects: GuardedEffects<'a>, prepared: PreparedCall) -> Self {
        Self { effects, prepared }
    }

    pub fn prepare(request: HookInvocationRequest) -> Result<PreparedCall, HookAdapterError> {
        let HookHandlerConfig::Command {
            command,
            args,
            shell,
            ..
        } = request.handler()
        else {
            return Err(HookAdapterError::new("hook handler is not a command"));
        };
        let argv = command_argv(command, args.as_deref(), shell.as_deref())?;
        let spec = SpawnSpec::new(argv, None, None, EnvPolicy::Inherited)
            .map_err(|error| HookAdapterError::new(error.to_string()))?;
        Ok(PreparedCall::new("hook_command", vec![Op::Spawn { spec }]))
    }

    pub async fn invoke(
        self,
        request: HookInvocationRequest,
    ) -> Result<HookAdapterResult, HookAdapterError> {
        let input = serde_json::to_vec(
            &HookHandlerRequestV1::from_runtime(&request)
                .map_err(|error| HookAdapterError::new(error.to_string()))?,
        )
        .map_err(|error| HookAdapterError::new(error.to_string()))?;
        if input.len() > MAX_HOOK_WIRE_BYTES {
            return Err(HookAdapterError::new("hook request exceeds 64 KiB"));
        }
        let spec = self
            .prepared
            .ops
            .iter()
            .find_map(|op| match op {
                Op::Spawn { spec } => Some(spec),
                _ => None,
            })
            .ok_or_else(|| HookAdapterError::new("command call has no spawn authority"))?;
        let settlement = match self
            .effects
            .exec_handler(spec, &input, request.effect_scope().clone())
            .await
        {
            Ok(output) => parse_process_output(output, DeliveryCertainty::DefinitelySent),
            Err(ProcessError::Exit {
                code: Some(2),
                result,
            }) => parse_veto_output(result),
            Err(error) => {
                HookAdapterSettlement::failed(error.to_string(), process_failure_delivery(&error))
            }
        };
        Ok(HookAdapterResult::Settled(settlement))
    }
}

fn command_argv(
    command: &str,
    args: Option<&[String]>,
    shell: Option<&str>,
) -> Result<Vec<OsString>, HookAdapterError> {
    match (args, shell) {
        (Some(_), Some(_)) => Err(HookAdapterError::new(
            "command args and shell are mutually exclusive",
        )),
        (Some(args), None) => Ok(std::iter::once(OsString::from(command))
            .chain(args.iter().map(OsString::from))
            .collect()),
        (None, Some("bash")) => Ok(vec![
            OsString::from("bash"),
            OsString::from("-c"),
            OsString::from(command),
        ]),
        (None, Some("powershell")) => Ok(vec![
            OsString::from("powershell"),
            OsString::from("-Command"),
            OsString::from(command),
        ]),
        (None, Some(_)) => Err(HookAdapterError::new("unsupported hook shell")),
        (None, None) => Ok(vec![OsString::from(command)]),
    }
}

fn parse_process_output(output: ToolResult, delivery: DeliveryCertainty) -> HookAdapterSettlement {
    let stdout = output.content.iter().find(|block| {
        matches!(
            block.provenance(),
            ToolOutputProvenance::Process {
                stream: StreamProvenance::Stdout,
                ..
            }
        )
    });
    let Some(stdout) = stdout else {
        return HookAdapterSettlement::failed("command hook returned no stdout", delivery);
    };
    if stdout.preview_is_head_and_tail()
        || stdout
            .evidence_reference()
            .is_some_and(|reference| reference.bytes > MAX_HOOK_WIRE_BYTES as u64)
    {
        return HookAdapterSettlement::failed("command hook result exceeds 64 KiB", delivery);
    }
    match HookHandlerResultV1::parse(stdout.text.as_bytes()) {
        Ok(result) => {
            HookAdapterSettlement::success(result, stdout.evidence_reference().cloned(), delivery)
        }
        Err(error) => {
            let mut settlement = HookAdapterSettlement::failed(error.to_string(), delivery);
            settlement.evidence = stdout.evidence_reference().cloned();
            settlement
        }
    }
}

fn parse_veto_output(output: ToolResult) -> HookAdapterSettlement {
    let stdout = output.content.iter().find(|block| {
        matches!(
            block.provenance(),
            ToolOutputProvenance::Process {
                stream: StreamProvenance::Stdout,
                ..
            }
        )
    });
    if stdout.is_none_or(|stdout| stdout.text.is_empty()) {
        return HookAdapterSettlement {
            decision: HookDecision::Block,
            feedback: None,
            failure: None,
            evidence: stdout.and_then(|stdout| stdout.evidence_reference().cloned()),
            delivery: DeliveryCertainty::DefinitelySent,
        };
    }
    let parsed = parse_process_output(output, DeliveryCertainty::DefinitelySent);
    if parsed.decision == HookDecision::Block && parsed.failure.is_none() {
        parsed
    } else {
        HookAdapterSettlement::failed(
            "command veto did not return a block result",
            DeliveryCertainty::DefinitelySent,
        )
    }
}

fn process_failure_delivery(error: &ProcessError) -> DeliveryCertainty {
    match error {
        ProcessError::Timeout { .. }
        | ProcessError::Cancelled
        | ProcessError::Exit { .. }
        | ProcessError::Signal { .. } => DeliveryCertainty::PossiblySent,
        _ => DeliveryCertainty::DefinitelyUnsent,
    }
}

pub struct HttpHookAdapter<'a> {
    effects: GuardedEffects<'a>,
    prepared: PreparedCall,
    request: HttpRequest,
}

impl<'a> HttpHookAdapter<'a> {
    pub fn new(effects: GuardedEffects<'a>, prepared: PreparedCall, request: HttpRequest) -> Self {
        Self {
            effects,
            prepared,
            request,
        }
    }

    pub fn prepare(request: HookInvocationRequest) -> Result<HttpRequest, HookAdapterError> {
        let HookHandlerConfig::Http {
            url,
            headers,
            allowed_env_vars,
            ..
        } = request.handler()
        else {
            return Err(HookAdapterError::new("hook handler is not HTTP"));
        };
        if url.contains("${") {
            return Err(HookAdapterError::new(
                "HTTP hook URL contains a secret token",
            ));
        }
        let url = url::Url::parse(url).map_err(|error| HookAdapterError::new(error.to_string()))?;
        let path = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        };
        let allowed = allowed_env_vars.as_deref().unwrap_or_default();
        let mut projected = Vec::with_capacity(headers.len() + 1);
        projected.push(
            HttpHeader::new("content-type", b"application/json".to_vec())
                .map_err(|error| HookAdapterError::new(error.to_string()))?,
        );
        for (name, value) in headers {
            if name.contains("${") {
                return Err(HookAdapterError::new(
                    "HTTP header name contains a secret token",
                ));
            }
            if let Some(secret_name) = exact_secret_token(value) {
                if !allowed.iter().any(|allowed| allowed == secret_name) {
                    return Err(HookAdapterError::new("HTTP secret source is not admitted"));
                }
                let value = std::env::var_os(secret_name)
                    .ok_or_else(|| HookAdapterError::new("HTTP secret source is missing"))?
                    .into_encoded_bytes();
                std::str::from_utf8(&value)
                    .map_err(|_| HookAdapterError::new("HTTP secret source is not UTF-8"))?;
                projected.push(
                    HttpHeader::secret(name, SecretMaterial::new(value))
                        .map_err(|error| HookAdapterError::new(error.to_string()))?,
                );
            } else {
                if value.contains("${") {
                    return Err(HookAdapterError::new(
                        "HTTP header has a non-exact secret token",
                    ));
                }
                projected.push(
                    HttpHeader::new(name, value.as_bytes().to_vec())
                        .map_err(|error| HookAdapterError::new(error.to_string()))?,
                );
            }
        }
        let body = serde_json::to_vec(
            &HookHandlerRequestV1::from_runtime(&request)
                .map_err(|error| HookAdapterError::new(error.to_string()))?,
        )
        .map_err(|error| HookAdapterError::new(error.to_string()))?;
        HttpRequest::new("POST", &path, projected, body)
            .map_err(|error| HookAdapterError::new(error.to_string()))
    }

    pub async fn invoke(
        self,
        runtime: HookInvocationRequest,
    ) -> Result<HookAdapterResult, HookAdapterError> {
        let settlement = match self
            .effects
            .guarded_hook_http(self.prepared, self.request, runtime.effect_scope().clone())
            .await
        {
            Ok(response) => settle_http_response(&self.effects, response).await,
            Err(error) => HookAdapterSettlement::failed(error.to_string(), error.delivery()),
        };
        Ok(HookAdapterResult::Settled(settlement))
    }
}

fn exact_secret_token(value: &str) -> Option<&str> {
    value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .filter(|name| !name.is_empty() && !name.contains('{') && !name.contains('}'))
}

async fn settle_http_response(
    effects: &GuardedEffects<'_>,
    response: OneHopResponse,
) -> HookAdapterSettlement {
    let (status, _, payload, delivery) = response.into_parts();
    if !(200..300).contains(&status) {
        return HookAdapterSettlement::failed("HTTP hook returned a non-success status", delivery);
    }
    let OneHopPayload::Response(mut body) = payload else {
        return HookAdapterSettlement::failed("HTTP hook returned a redirect", delivery);
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(chunk) if bytes.len().saturating_add(chunk.len()) <= MAX_HOOK_WIRE_BYTES => {
                bytes.extend_from_slice(&chunk);
            }
            Ok(_) => {
                return HookAdapterSettlement::failed("HTTP hook result exceeds 64 KiB", delivery);
            }
            Err(error) => {
                return HookAdapterSettlement::failed(error.to_string(), error.delivery());
            }
        }
    }
    let evidence = match effects.store_handler_evidence(&bytes) {
        Ok(evidence) => evidence,
        Err(error) => return HookAdapterSettlement::failed(error.to_string(), delivery),
    };
    match HookHandlerResultV1::parse(&bytes) {
        Ok(result) => HookAdapterSettlement::success(result, Some(evidence), delivery),
        Err(error) => {
            let mut settlement = HookAdapterSettlement::failed(error.to_string(), delivery);
            settlement.evidence = Some(evidence);
            settlement
        }
    }
}
