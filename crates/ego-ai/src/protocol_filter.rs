//! Strict byte exchange at the user-owned protocol-filter boundary.

use crate::http::HttpRequest;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ego_core::config::{ProtocolFilterConfig, is_protocol_filter_identity_header};
use ego_core::event::id::SessionId;
use ego_core::generator::attempt::AttemptRef;
use ego_core::generator::filter_state::{
    FilterStateExchange, FilterStateSnapshot, FilterVariables,
};
use ego_core::generator::port::AiEvent;
use ego_core::permit::HttpHeader;
use futures::channel::mpsc::UnboundedSender;
use futures::future::BoxFuture;
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashSet};

pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ENVELOPE_BYTES: usize = 22_435_160;

/// OS execution is injected; no credential or request headers cross this port.
pub trait ProtocolFilterCommand: Send + Sync {
    fn run(&self, input: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, &'static str>>;
    fn open_response(&self) -> Result<Box<dyn ProtocolFilterResponse>, &'static str>;
    fn check_live(&self) -> Result<(), &'static str> {
        Ok(())
    }
}

/// One process retains parsing context across response frames, without writing
/// session variables. Dropping it must cancel and reap the process tree.
pub trait ProtocolFilterResponse: Send {
    fn exchange(&mut self, input: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, &'static str>>;
    fn finish(&mut self) -> BoxFuture<'_, Result<(), &'static str>>;
}

#[derive(Serialize)]
struct Input<'a> {
    version: u32,
    direction: &'static str,
    provider: &'a str,
    model: &'a str,
    session_id: SessionId,
    generation_id: ego_core::event::id::GenerationId,
    attempt_id: ego_core::event::id::AttemptId,
    body_base64: String,
    state: &'a FilterVariables,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    version: u32,
    body_base64: String,
    identity_headers: Vec<IdentityHeader>,
    expected_revision: u64,
    #[serde(deserialize_with = "unique_writes")]
    state_writes: BTreeMap<String, String>,
}

fn unique_writes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    struct Writes;
    impl<'de> serde::de::Visitor<'de> for Writes {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("unique string session variables")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if values.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate session variable"));
                }
                if values.len() > 64 {
                    return Err(serde::de::Error::custom("too many session variables"));
                }
            }
            Ok(values)
        }
    }
    deserializer.deserialize_map(Writes)
}

async fn exchange(
    events: &UnboundedSender<AiEvent>,
    call: (
        FilterStateExchange,
        futures::channel::oneshot::Receiver<Result<FilterStateSnapshot, &'static str>>,
    ),
) -> Result<FilterStateSnapshot, &'static str> {
    let (request, reply) = call;
    events
        .unbounded_send(AiEvent::FilterState(request))
        .map_err(|_| "protocol_filter requires a session journal owner")?;
    reply
        .await
        .map_err(|_| "protocol_filter journal acknowledgement missing")?
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityHeader {
    name: String,
    value: String,
}

pub(crate) async fn apply(
    config: &ProtocolFilterConfig,
    command: &dyn ProtocolFilterCommand,
    provider: &str,
    model: &str,
    attempt: AttemptRef,
    request: &mut HttpRequest,
    events: &UnboundedSender<AiEvent>,
) -> Result<ResponseFilter, &'static str> {
    config.validate()?;
    // Stable identity of the user-configured executable, independently scoped
    // by provider and the session owner's journal. No filesystem state here.
    let namespace = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(provider, &config.command))
                .map_err(|_| "protocol_filter identity encoding failed")?
        )
    );
    let snapshot = exchange(events, FilterStateExchange::read(namespace.clone())).await?;
    let body = request
        .body
        .as_ref()
        .ok_or("protocol_filter requires a body request")?;
    if body.is_empty() || body.len() > MAX_BODY_BYTES {
        return Err("protocol_filter input body exceeds limits");
    }
    let input = serde_json::to_vec(&Input {
        version: 1,
        direction: "request",
        provider,
        model,
        session_id: snapshot.session_id,
        generation_id: attempt.generation(),
        attempt_id: attempt.attempt(),
        body_base64: STANDARD.encode(body),
        state: &snapshot.state,
    })
    .map_err(|_| "protocol_filter input encoding failed")?;
    if input.len() > MAX_ENVELOPE_BYTES {
        return Err("protocol_filter input exceeds limits");
    }
    let output = command.run(input).await?;
    if output.len() > MAX_ENVELOPE_BYTES {
        return Err("protocol_filter output exceeds limits");
    }
    let output: Output =
        serde_json::from_slice(&output).map_err(|_| "protocol_filter returned invalid v1 JSON")?;
    if output.version != 1 {
        return Err("protocol_filter returned unsupported version");
    }
    if output.expected_revision != snapshot.state.revision {
        return Err("protocol_filter state revision conflict");
    }
    snapshot.state.proposed(&output.state_writes)?;
    if output.body_base64.len() > MAX_BODY_BYTES.div_ceil(3) * 4 {
        return Err("protocol_filter output body exceeds limits");
    }
    let body = STANDARD
        .decode(&output.body_base64)
        .map_err(|_| "protocol_filter returned invalid base64")?;
    if body.is_empty() || body.len() > MAX_BODY_BYTES {
        return Err("protocol_filter output body exceeds limits");
    }
    if output.identity_headers.len() > 16 {
        return Err("protocol_filter returned too many headers");
    }
    let mut seen = HashSet::new();
    let mut headers = Vec::new();
    for header in output.identity_headers {
        let name = header.name.to_ascii_lowercase();
        if !is_protocol_filter_identity_header(&name)
            || !config
                .identity_headers
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&name))
            || !seen.insert(name.clone())
            || header.value.len() > 4096
            || !header
                .value
                .bytes()
                .all(|byte| byte == b'\t' || (32..=126).contains(&byte))
            || request
                .headers
                .iter()
                .any(|existing| existing.name() == name && existing.is_secret())
        {
            return Err(
                "protocol_filter returned a forbidden, undeclared, duplicate or invalid header",
            );
        }
        headers.push(
            HttpHeader::new(&name, header.value.into_bytes())
                .map_err(|_| "protocol_filter returned an invalid header")?,
        );
    }
    // Even an empty write proposal must validate the revision before dispatch:
    // the returned bytes may depend on the snapshot the filter observed.
    let accepted = exchange(
        events,
        FilterStateExchange::save(namespace, output.expected_revision, output.state_writes),
    )
    .await?;
    command.check_live()?;
    let response_filter = ResponseFilter {
        owner: command.open_response()?,
        provider: provider.to_owned(),
        model: model.to_owned(),
        attempt,
        snapshot: accepted,
        sequence: 0,
    };
    // Commit only a completely validated and durably accepted exchange.
    request
        .headers
        .retain(|header| !seen.contains(header.name()));
    request.headers.extend(headers);
    request.body = Some(body);
    Ok(response_filter)
}

pub(crate) struct ResponseFilter {
    owner: Box<dyn ProtocolFilterResponse>,
    provider: String,
    model: String,
    attempt: AttemptRef,
    snapshot: FilterStateSnapshot,
    sequence: u64,
}

#[derive(Serialize)]
struct ResponseInput<'a> {
    #[serde(flatten)]
    context: Input<'a>,
    sequence: u64,
    eof: bool,
    status: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseOutput {
    version: u32,
    sequence: u64,
    body_base64: String,
}

impl ResponseFilter {
    async fn frame(
        &mut self,
        body: &[u8],
        eof: bool,
        status: u16,
    ) -> Result<Vec<u8>, &'static str> {
        if body.len() > MAX_BODY_BYTES {
            return Err("protocol_filter response input exceeds limits");
        }
        let input = serde_json::to_vec(&ResponseInput {
            context: Input {
                version: 1,
                direction: "response",
                provider: &self.provider,
                model: &self.model,
                session_id: self.snapshot.session_id,
                generation_id: self.attempt.generation(),
                attempt_id: self.attempt.attempt(),
                body_base64: STANDARD.encode(body),
                state: &self.snapshot.state,
            },
            sequence: self.sequence,
            eof,
            status,
        })
        .map_err(|_| "protocol_filter response encoding failed")?;
        if input.len() > MAX_ENVELOPE_BYTES {
            return Err("protocol_filter response envelope exceeds limits");
        }
        let output = self.owner.exchange(input).await?;
        if output.len() > MAX_ENVELOPE_BYTES {
            return Err("protocol_filter response output exceeds limits");
        }
        let output: ResponseOutput = serde_json::from_slice(&output)
            .map_err(|_| "protocol_filter returned invalid response JSON")?;
        if output.version != 1 || output.sequence != self.sequence {
            return Err("protocol_filter response sequence or version mismatch");
        }
        if output.body_base64.len() > MAX_BODY_BYTES.div_ceil(3) * 4 {
            return Err("protocol_filter response body exceeds limits");
        }
        let bytes = STANDARD
            .decode(output.body_base64)
            .map_err(|_| "protocol_filter returned invalid response base64")?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err("protocol_filter response body exceeds limits");
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("protocol_filter response sequence exhausted")?;
        if eof {
            self.owner.finish().await?;
        }
        Ok(bytes)
    }

    pub(crate) async fn wrap(
        self,
        response: crate::http::HttpResponse,
    ) -> Result<crate::http::HttpResponse, crate::http::HttpError> {
        let status = response.status;
        let mut body = stream::unfold(
            (response.body, self, false),
            move |(mut body, mut filter, done)| async move {
                if done {
                    return None;
                }
                let next = body.next().await;
                let eof = next.is_none();
                let bytes = match next {
                    Some(Err(error)) => return Some((Err(error), (body, filter, true))),
                    Some(Ok(bytes)) => bytes,
                    None => bytes::Bytes::new(),
                };
                let result = filter
                    .frame(&bytes, eof, status)
                    .await
                    .map(bytes::Bytes::from)
                    .map_err(crate::http::HttpError::protocol_filter);
                let done = eof || result.is_err();
                Some((result, (body, filter, done)))
            },
        )
        .boxed();
        // Error decoders intentionally stop at a diagnostic cap. Validate the
        // entire bounded filtered error response before handing it to them.
        if status != 200 {
            let mut diagnostic = Vec::new();
            while let Some(chunk) = body.next().await {
                let chunk = chunk?;
                if diagnostic.len().saturating_add(chunk.len()) > 64 * 1024 {
                    return Err(crate::http::HttpError::protocol_filter(
                        "protocol_filter error response exceeds limits",
                    ));
                }
                diagnostic.extend_from_slice(&chunk);
            }
            body = stream::once(futures::future::ready(Ok(bytes::Bytes::from(diagnostic)))).boxed();
        }
        Ok(crate::http::HttpResponse {
            status,
            headers: response.headers,
            body,
        })
    }
}
