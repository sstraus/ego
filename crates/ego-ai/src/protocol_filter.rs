//! Strict byte exchange at the user-owned protocol-filter boundary.

use crate::http::HttpRequest;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ego_core::config::ProtocolFilterConfig;
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

const TIMEOUT_RETRIES: usize = 3;

fn retry_timeout(retries: &mut usize) -> bool {
    if *retries == TIMEOUT_RETRIES {
        return false;
    }
    *retries += 1;
    true
}

pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ENVELOPE_BYTES: usize = 22_435_160;

/// Data-free failures reported by the child execution port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildFailure {
    Spawn,
    Io,
    Framing,
    EarlyEof,
    OutputLimit,
    CommandTimeout,
    RunTimeout,
    InvalidResult,
    Cancelled,
    Exit,
    CleanupUncertain,
    /// A host-side fault (session journal, local notice sink), not the filter's.
    Host,
}

impl ChildFailure {
    fn diagnostic(self) -> (&'static str, &'static str) {
        match self {
            Self::Spawn => ("spawn", "check that the configured filter is executable"),
            Self::Io => ("io", "check the filter's stdin and stdout handling"),
            Self::Framing => ("framing", "write exactly one V1 JSON line per exchange"),
            Self::EarlyEof => (
                "early EOF",
                "keep stdout open until the complete result line is sent",
            ),
            Self::OutputLimit => (
                "output limit",
                "return a smaller result within the V1 limits",
            ),
            Self::CommandTimeout => (
                "timeout",
                "check why the filter did not complete within its timeout",
            ),
            Self::RunTimeout => (
                "timeout",
                "check the run deadline before starting a new run",
            ),
            Self::InvalidResult => (
                "invalid result",
                "check the filter result against the V1 contract",
            ),
            Self::Cancelled => (
                "cancelled",
                "start a new run if the cancellation was unintended",
            ),
            Self::Exit => ("exit", "check why the filter exits unsuccessfully"),
            Self::CleanupUncertain => (
                "cleanup uncertain",
                "check whether the filter is still running before a new run",
            ),
            Self::Host => (
                "host",
                "check the local session journal, the filter is not at fault",
            ),
        }
    }

    // Egress uses the existing static diagnostic port. Only the launch and
    // request phases occur there.
    fn egress_message(self, phase: Phase) -> &'static str {
        macro_rules! message {
            ($category:literal, $action:literal) => {
                if phase == Phase::Launch {
                    concat!("protocol_filter launch: ", $category, " (", $action, ")")
                } else {
                    concat!("protocol_filter request: ", $category, " (", $action, ")")
                }
            };
        }
        match self {
            Self::Spawn => {
                "protocol_filter launch: spawn (check that the configured filter is executable)"
            }
            Self::Io => message!("io", "check the filter's stdin and stdout handling"),
            Self::Framing => message!("framing", "write exactly one V1 JSON line per exchange"),
            Self::EarlyEof => message!(
                "early EOF",
                "keep stdout open until the complete result line is sent"
            ),
            Self::OutputLimit => message!(
                "output limit",
                "return a smaller result within the V1 limits"
            ),
            Self::CommandTimeout => "protocol_filter timed out",
            Self::RunTimeout => "protocol_filter run deadline expired",
            Self::InvalidResult => "protocol_filter returned an invalid result",
            Self::Cancelled => "protocol_filter cancelled",
            Self::Exit => "protocol_filter exited unsuccessfully",
            Self::CleanupUncertain => "protocol_filter process-tree cleanup uncertain",
            Self::Host => "protocol_filter host failure (the filter is not at fault)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Launch,
    Hello,
    Request,
    Response,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::Hello => "hello",
            Self::Request => "request",
            Self::Response => "response",
        }
    }

    fn failure(self, category: ChildFailure) -> FilterFailure {
        FilterFailure::Exchange {
            phase: self,
            category,
            detail: None,
        }
    }

    fn invalid(self, detail: &'static str) -> FilterFailure {
        FilterFailure::Exchange {
            phase: self,
            category: ChildFailure::InvalidResult,
            detail: Some(detail),
        }
    }

    fn host(self, detail: &'static str) -> FilterFailure {
        FilterFailure::Exchange {
            phase: self,
            category: ChildFailure::Host,
            detail: Some(detail),
        }
    }

    fn notice(self, error: NoticeError) -> FilterFailure {
        match error {
            NoticeError::Limits => self.invalid(error.message()),
            NoticeError::SinkClosed => self.host(error.message()),
        }
    }

    fn result(self, error: FilterFailure) -> FilterFailure {
        match error {
            FilterFailure::Invalid(detail) => self.invalid(detail),
            _ => error,
        }
    }
}

/// Typed policy rejection must not be mistaken for a retryable/fallback failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilterFailure {
    Invalid(&'static str),
    Exchange {
        phase: Phase,
        category: ChildFailure,
        detail: Option<&'static str>,
    },
    Rejected,
}

impl FilterFailure {
    pub(crate) fn message(self) -> String {
        match self {
            Self::Exchange {
                phase,
                category,
                detail,
            } => {
                let (category, action) = category.diagnostic();
                let phase = phase.label();
                if let Some(detail) = detail {
                    let detail = detail.strip_prefix("protocol_filter ").unwrap_or(detail);
                    format!("protocol_filter {phase}: {category} ({detail}; {action})")
                } else {
                    format!("protocol_filter {phase}: {category} ({action})")
                }
            }
            _ => self.static_message().to_owned(),
        }
    }
    fn static_message(self) -> &'static str {
        match self {
            Self::Invalid(message) => message,
            Self::Rejected => "protocol_filter policy rejected this attempt",
            Self::Exchange {
                phase, category, ..
            } => category.egress_message(phase),
        }
    }
    pub(crate) fn kind(self) -> ego_core::generator::types::ErrorKind {
        match self {
            Self::Invalid(_) | Self::Exchange { .. } => {
                ego_core::generator::types::ErrorKind::Invalid
            }
            Self::Rejected => ego_core::generator::types::ErrorKind::PolicyRejected,
        }
    }
}
impl From<&'static str> for FilterFailure {
    fn from(message: &'static str) -> Self {
        Self::Invalid(message)
    }
}

/// OS execution is injected; no credential or request headers cross this port.
pub trait ProtocolFilterCommand: Send + Sync {
    fn open_response(&self) -> Result<Box<dyn ProtocolFilterResponse>, ChildFailure>;
    fn check_live(&self) -> Result<(), ChildFailure> {
        Ok(())
    }
}

/// One process retains parsing context across response frames, without writing
/// session variables. Dropping it must cancel and reap the process tree.
pub trait ProtocolFilterResponse: Send {
    fn exchange(&mut self, input: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, ChildFailure>>;
    fn finish(&mut self) -> BoxFuture<'_, Result<(), ChildFailure>>;
}

#[derive(Serialize)]
struct Input<'a> {
    version: u32,
    direction: &'static str,
    provider: &'a str,
    model: &'a str,
    session_id: SessionId,
    turn_index: u64,
    generation_id: ego_core::event::id::GenerationId,
    attempt_id: ego_core::event::id::AttemptId,
    body_base64: String,
    state: &'a FilterVariables,
}

#[derive(Serialize)]
struct Hello<'a> {
    generation_role: ego_core::event::domain::GenerationRole,
    wire_format: &'a str,
    version: u32,
    direction: &'static str,
    provider: &'a str,
    model: &'a str,
    session_id: SessionId,
    turn_index: u64,
    generation_id: ego_core::event::id::GenerationId,
    attempt_id: ego_core::event::id::AttemptId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Interest {
    #[serde(default)]
    notices: Vec<String>,
    version: u32,
    request: bool,
    response: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    #[serde(default)]
    notices: Vec<String>,
    version: u32,
    body_base64: String,
    identity_headers: Vec<IdentityHeader>,
    expected_revision: u64,
    #[serde(deserialize_with = "unique_writes")]
    state_writes: BTreeMap<String, String>,
}

/// Rejection is distinct from a transformed body; extra fields are refused.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rejection {
    version: u32,
    reject: bool,
    #[serde(default)]
    sequence: Option<u64>,
    #[serde(default)]
    notices: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Decision<T> {
    Accept(T),
    Reject(Rejection),
}

/// At most four local notices per attempt, across startup/request/response.
struct NoticeSink {
    events: UnboundedSender<AiEvent>,
    remaining: usize,
    pending: Option<Vec<String>>,
}

/// A filter that sends too many notices is at fault; a closed sink is the host's.
enum NoticeError {
    Limits,
    SinkClosed,
}

impl NoticeError {
    fn message(&self) -> &'static str {
        match self {
            Self::Limits => "protocol_filter notices exceed limits",
            Self::SinkClosed => "protocol_filter local notice sink closed",
        }
    }
}

impl From<NoticeError> for FilterFailure {
    fn from(error: NoticeError) -> Self {
        Self::Invalid(error.message())
    }
}

impl NoticeSink {
    fn publish(&mut self, values: &[String]) -> Result<(), NoticeError> {
        if values.len() > 4 || values.iter().any(|value| value.len() > 1024) {
            return Err(NoticeError::Limits);
        }
        for value in values {
            if self.remaining == 0 {
                break;
            }
            let mut clean = String::new();
            for ch in value.chars().filter(|ch| {
                !ch.is_control()
                    && !matches!(ch,
                '\u{00ad}' | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' |
                '\u{2060}'..='\u{206f}' | '\u{feff}')
            }) {
                if clean.len() + ch.len_utf8() > 256 {
                    break;
                }
                clean.push(ch);
            }
            if !clean.trim().is_empty() {
                if self.events.is_closed() {
                    return Err(NoticeError::SinkClosed);
                }
                if let Some(pending) = &mut self.pending {
                    pending.push(clean);
                } else {
                    self.events
                        .unbounded_send(AiEvent::FilterNotice { text: clean })
                        .map_err(|_| NoticeError::SinkClosed)?;
                }
                self.remaining -= 1;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), &'static str> {
        for text in self.pending.take().unwrap_or_default() {
            self.events
                .unbounded_send(AiEvent::FilterNotice { text })
                .map_err(|_| "protocol_filter local notice sink closed")?;
        }
        Ok(())
    }
}

fn decision<T>(
    bytes: &[u8],
    notices: &mut NoticeSink,
    sequence: Option<u64>,
) -> Result<T, FilterFailure>
where
    T: for<'de> Deserialize<'de>,
{
    match serde_json::from_slice::<Decision<T>>(bytes)
        .map_err(|_| "protocol_filter returned invalid v1 JSON")?
    {
        Decision::Accept(value) => Ok(value),
        Decision::Reject(rejection) => {
            if rejection.version != 1 || !rejection.reject || rejection.sequence != sequence {
                return Err("protocol_filter returned invalid rejection".into());
            }
            // A bad diagnostic cannot turn a policy denial into fallback permission.
            let _ = notices.publish(&rejection.notices);
            Err(FilterFailure::Rejected)
        }
    }
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

#[derive(Clone, Copy)]
pub(crate) struct FilterIdentity<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub generation_role: ego_core::event::domain::GenerationRole,
    pub wire_format: &'a str,
}

/// Same provider/executable namespace for provider filtering and request-only egress.
pub fn namespace(provider: &str, config: &ProtocolFilterConfig) -> Result<String, &'static str> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(provider, &config.command))
                .map_err(|_| "protocol_filter identity encoding failed")?
        )
    ))
}

/// One request-only exchange. It cannot write session variables or identity headers.
pub async fn apply_egress(
    command: &dyn ProtocolFilterCommand,
    identity: (&str, &str),
    snapshot: &FilterStateSnapshot,
    request: ego_core::egress::EgressRequest<'_>,
    events: &UnboundedSender<AiEvent>,
) -> Result<(), &'static str> {
    let mut retries = 0;
    loop {
        command
            .check_live()
            .map_err(|category| category.egress_message(Phase::Launch))?;
        let mut notices = NoticeSink {
            events: events.clone(),
            remaining: 4,
            pending: Some(Vec::new()),
        };
        match apply_egress_once(command, identity, snapshot, &request, &mut notices).await {
            Err(FilterFailure::Exchange {
                category: ChildFailure::CommandTimeout,
                ..
            }) if retry_timeout(&mut retries) => continue,
            Ok(value) => {
                notices.flush()?;
                return Ok(value);
            }
            Err(error) => {
                // Preserve the primary failure, especially a policy rejection.
                let _ = notices.flush();
                return Err(error.static_message());
            }
        }
    }
}

async fn apply_egress_once(
    command: &dyn ProtocolFilterCommand,
    identity: (&str, &str),
    snapshot: &FilterStateSnapshot,
    request: &ego_core::egress::EgressRequest<'_>,
    notices: &mut NoticeSink,
) -> Result<(), FilterFailure> {
    let body = serde_json::to_vec(&request.payload)
        .map_err(|_| "protocol_filter egress encoding failed")?;
    if body.is_empty() || body.len() > MAX_BODY_BYTES {
        return Err("protocol_filter egress body exceeds limits".into());
    }
    let input = serde_json::to_vec(&serde_json::json!({
        "version":1, "direction":request.direction,
        "provider":identity.0, "model":identity.1,
        "session_id":snapshot.session_id, "turn_index":snapshot.turn_index,
        "destination":request.destination, "body_base64":STANDARD.encode(body),
        "state":snapshot.state,
    }))
    .map_err(|_| "protocol_filter egress encoding failed")?;
    if input.len() > MAX_ENVELOPE_BYTES {
        return Err("protocol_filter egress envelope exceeds limits".into());
    }
    let mut owner = command
        .open_response()
        .map_err(|category| Phase::Launch.failure(category))?;
    let output = owner
        .exchange(input)
        .await
        .map_err(|category| Phase::Request.failure(category))?;
    if output.len() > MAX_ENVELOPE_BYTES {
        return Err("protocol_filter egress envelope exceeds limits".into());
    }
    let output: Output = decision(&output, notices, None)?;
    if output.version != 1
        || output.expected_revision != snapshot.state.revision
        || !output.identity_headers.is_empty()
        || !output.state_writes.is_empty()
    {
        return Err("protocol_filter egress must preserve read-only state and headers".into());
    }
    notices.publish(&output.notices)?;
    if output.body_base64.len() > MAX_BODY_BYTES.div_ceil(3) * 4 {
        return Err("protocol_filter egress body exceeds limits".into());
    }
    let body = STANDARD
        .decode(output.body_base64)
        .map_err(|_| "protocol_filter returned invalid base64")?;
    if body.is_empty() || body.len() > MAX_BODY_BYTES {
        return Err("protocol_filter egress body exceeds limits".into());
    }
    let payload: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|_| "protocol_filter returned invalid egress JSON")?;
    // Accept or reject only: the admitted request is dispatched unchanged.
    if payload != request.payload {
        let _ = notices.publish(&["policy rejected transformed egress payload".to_owned()]);
        return Err("protocol_filter policy rejected transformed egress payload".into());
    }
    owner
        .finish()
        .await
        .map_err(|category| Phase::Request.failure(category))?;
    command
        .check_live()
        .map_err(|category| Phase::Request.failure(category))?;
    Ok(())
}

pub(crate) async fn apply(
    config: &ProtocolFilterConfig,
    command: &dyn ProtocolFilterCommand,
    identity: FilterIdentity<'_>,
    attempt: AttemptRef,
    request: &mut HttpRequest,
    events: &UnboundedSender<AiEvent>,
) -> Result<Option<ResponseFilter>, FilterFailure> {
    let mut retries = 0;
    loop {
        command
            .check_live()
            .map_err(|category| Phase::Launch.failure(category))?;
        let mut notices = NoticeSink {
            events: events.clone(),
            remaining: 4,
            pending: Some(Vec::new()),
        };
        let mut retry_safe = true;
        match apply_once(
            config,
            command,
            identity,
            attempt,
            request,
            &mut notices,
            &mut retry_safe,
        )
        .await
        {
            Err(FilterFailure::Exchange {
                category: ChildFailure::CommandTimeout,
                ..
            }) if retry_safe && retry_timeout(&mut retries) => {
                continue;
            }
            Ok(value) => {
                notices.flush()?;
                return Ok(value);
            }
            Err(error) => {
                // Preserve the primary failure, especially a policy rejection.
                let _ = notices.flush();
                return Err(error);
            }
        }
    }
}

async fn apply_once(
    config: &ProtocolFilterConfig,
    command: &dyn ProtocolFilterCommand,
    identity: FilterIdentity<'_>,
    attempt: AttemptRef,
    request: &mut HttpRequest,
    notices: &mut NoticeSink,
    retry_safe: &mut bool,
) -> Result<Option<ResponseFilter>, FilterFailure> {
    let FilterIdentity {
        provider,
        model,
        generation_role,
        wire_format,
    } = identity;
    let events = &notices.events.clone();
    config.validate()?;
    // Stable identity of the user-configured executable, independently scoped
    // by provider and the session owner's journal. No filesystem state here.
    let namespace = namespace(provider, config)?;
    let snapshot = exchange(events, FilterStateExchange::read(namespace.clone())).await?;
    let mut owner = command
        .open_response()
        .map_err(|category| Phase::Launch.failure(category))?;
    let hello = serde_json::to_vec(&Hello {
        generation_role,
        wire_format,
        version: 1,
        direction: "hello",
        provider,
        model,
        session_id: snapshot.session_id,
        turn_index: snapshot.turn_index,
        generation_id: attempt.generation(),
        attempt_id: attempt.attempt(),
    })
    .map_err(|_| "protocol_filter hello encoding failed")?;
    let hello_phase = Phase::Hello;
    let declared = owner
        .exchange(hello)
        .await
        .map_err(|category| hello_phase.failure(category))?;
    let interest: Interest =
        decision(&declared, notices, None).map_err(|error| hello_phase.result(error))?;
    if interest.version != 1 {
        return Err(hello_phase.invalid("unsupported interest version"));
    }
    notices
        .publish(&interest.notices)
        .map_err(|error| hello_phase.notice(error))?;
    let mut accepted = snapshot;
    let phase = if interest.request {
        Phase::Request
    } else {
        Phase::Hello
    };
    if interest.request {
        let body = request
            .body
            .as_ref()
            .ok_or("protocol_filter requires a body request")?;
        if body.is_empty() || body.len() > MAX_BODY_BYTES {
            return Err("protocol_filter input body exceeds limits".into());
        }
        let input = serde_json::to_vec(&Input {
            version: 1,
            direction: "request",
            provider,
            model,
            session_id: accepted.session_id,
            turn_index: accepted.turn_index,
            generation_id: attempt.generation(),
            attempt_id: attempt.attempt(),
            body_base64: STANDARD.encode(body),
            state: &accepted.state,
        })
        .map_err(|_| "protocol_filter input encoding failed")?;
        if input.len() > MAX_ENVELOPE_BYTES {
            return Err("protocol_filter input exceeds limits".into());
        }
        let output = owner
            .exchange(input)
            .await
            .map_err(|category| phase.failure(category))?;
        if output.len() > MAX_ENVELOPE_BYTES {
            return Err(phase.failure(ChildFailure::OutputLimit));
        }
        let output: Output =
            decision(&output, notices, None).map_err(|error| phase.result(error))?;
        if output.version != 1 {
            return Err(phase.invalid("protocol_filter returned unsupported version"));
        }
        notices
            .publish(&output.notices)
            .map_err(|error| phase.notice(error))?;
        if output.expected_revision != accepted.state.revision {
            return Err(phase.invalid("protocol_filter state revision conflict"));
        }
        accepted
            .state
            .proposed(&output.state_writes)
            .map_err(|detail| phase.invalid(detail))?;
        if output.body_base64.len() > MAX_BODY_BYTES.div_ceil(3) * 4 {
            return Err(phase.failure(ChildFailure::OutputLimit));
        }
        let body = STANDARD
            .decode(&output.body_base64)
            .map_err(|_| phase.invalid("invalid base64"))?;
        if body.is_empty() {
            return Err(phase.invalid("empty request body"));
        }
        if body.len() > MAX_BODY_BYTES {
            return Err(phase.failure(ChildFailure::OutputLimit));
        }
        if output.identity_headers.len() > 16 {
            return Err(phase.invalid("protocol_filter returned too many headers"));
        }
        let mut seen = HashSet::new();
        let mut headers = Vec::new();
        for header in output.identity_headers {
            let name = header.name.to_ascii_lowercase();
            if name.len() > 128
                || !seen.insert(name.clone())
                || header.value.len() > 4096
                || !header
                    .value
                    .bytes()
                    .all(|byte| byte == b'\t' || (32..=126).contains(&byte))
            {
                return Err(phase.invalid("protocol_filter returned a duplicate or invalid header"));
            }
            headers.push(
                HttpHeader::new(&name, header.value.into_bytes())
                    .map_err(|_| phase.invalid("invalid header"))?,
            );
        }
        // Even an empty write proposal must validate the revision before dispatch:
        // the returned bytes may depend on the snapshot the filter observed.
        // Crossing the journal boundary is irreversible even if a later finish
        // times out. Do not replay an accepted state proposal or transformed body.
        *retry_safe = false;
        accepted = exchange(
            events,
            FilterStateExchange::save(namespace, output.expected_revision, output.state_writes),
        )
        .await
        .map_err(|detail| phase.host(detail))?;
        // Commit only a completely validated and durably accepted exchange.
        request
            .headers
            .retain(|header| !seen.contains(header.name()));
        request.headers.extend(headers);
        request.body = Some(body);
    }
    command
        .check_live()
        .map_err(|category| phase.failure(category))?;
    if interest.response {
        Ok(Some(ResponseFilter {
            owner,
            notices: NoticeSink {
                events: notices.events.clone(),
                remaining: notices.remaining,
                pending: None,
            },
            provider: provider.to_owned(),
            model: model.to_owned(),
            attempt,
            snapshot: accepted,
            sequence: 0,
        }))
    } else {
        owner
            .finish()
            .await
            .map_err(|category| phase.failure(category))?;
        command
            .check_live()
            .map_err(|category| phase.failure(category))?;
        Ok(None)
    }
}

pub(crate) struct ResponseFilter {
    notices: NoticeSink,
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
    #[serde(default)]
    notices: Vec<String>,
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
    ) -> Result<Vec<u8>, FilterFailure> {
        let phase = Phase::Response;
        if body.len() > MAX_BODY_BYTES {
            return Err("protocol_filter response input exceeds limits".into());
        }
        let input = serde_json::to_vec(&ResponseInput {
            context: Input {
                version: 1,
                direction: "response",
                provider: &self.provider,
                model: &self.model,
                session_id: self.snapshot.session_id,
                turn_index: self.snapshot.turn_index,
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
            return Err("protocol_filter response envelope exceeds limits".into());
        }
        let output = self
            .owner
            .exchange(input)
            .await
            .map_err(|category| phase.failure(category))?;
        if output.len() > MAX_ENVELOPE_BYTES {
            return Err(phase.failure(ChildFailure::OutputLimit));
        }
        let output: ResponseOutput = decision(&output, &mut self.notices, Some(self.sequence))
            .map_err(|error| phase.result(error))?;
        if output.version != 1 || output.sequence != self.sequence {
            return Err(phase.invalid("response sequence or version mismatch"));
        }
        self.notices
            .publish(&output.notices)
            .map_err(|error| phase.notice(error))?;
        if output.body_base64.len() > MAX_BODY_BYTES.div_ceil(3) * 4 {
            return Err(phase.failure(ChildFailure::OutputLimit));
        }
        let bytes = STANDARD
            .decode(output.body_base64)
            .map_err(|_| phase.invalid("invalid response base64"))?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err(phase.failure(ChildFailure::OutputLimit));
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("protocol_filter response sequence exhausted")?;
        if eof {
            self.owner
                .finish()
                .await
                .map_err(|category| phase.failure(category))?;
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
                        Phase::Response.failure(ChildFailure::OutputLimit),
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
