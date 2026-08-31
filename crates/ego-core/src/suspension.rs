//! Durable foreign-operation suspension and continuation journal port.
//!
//! Core owns the state machine and serializable record vocabulary. Adapters
//! persist records and events; they do not reinterpret authority or time.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::config::Mode;
use crate::event::time::{Clock, Timestamp};
use crate::permit::{Authority, Authorized, PreparedCall};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationKind {
    Tool,
    Resource,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationSubject {
    kind: ContinuationKind,
    server: String,
    operation: String,
    admission_generation: String,
    descriptor_fingerprint: String,
}

impl ContinuationSubject {
    pub fn new(
        server: impl Into<String>,
        operation: impl Into<String>,
        admission_generation: impl Into<String>,
        descriptor_fingerprint: impl Into<String>,
    ) -> Result<Self, ContinuationError> {
        let operation = operation.into();
        let kind = match operation.as_str() {
            "tools/call" => ContinuationKind::Tool,
            "resources/read" => ContinuationKind::Resource,
            "prompts/get" => ContinuationKind::Prompt,
            _ => return Err(ContinuationError::UnsupportedSubject(operation)),
        };
        let subject = Self {
            kind,
            server: server.into(),
            operation,
            admission_generation: admission_generation.into(),
            descriptor_fingerprint: descriptor_fingerprint.into(),
        };
        if [
            &subject.server,
            &subject.admission_generation,
            &subject.descriptor_fingerprint,
        ]
        .into_iter()
        .any(|part| !valid_value(part))
        {
            return Err(ContinuationError::InvalidSubject);
        }
        Ok(subject)
    }

    pub fn kind(&self) -> ContinuationKind {
        self.kind
    }
    pub fn server(&self) -> &str {
        &self.server
    }
    pub fn operation(&self) -> &str {
        &self.operation
    }
    pub fn admission_generation(&self) -> &str {
        &self.admission_generation
    }
    pub fn descriptor_fingerprint(&self) -> &str {
        &self.descriptor_fingerprint
    }
}

pub trait ResumeKeySource: Send + Sync {
    fn resume_key(&self) -> Result<Arc<[u8]>, ContinuationError>;
}

/// Core-owned preparation of one sealed resume authority before foreign I/O.
pub struct ResumeSealer {
    keys: Arc<dyn ResumeKeySource>,
    clock: Arc<dyn Clock>,
    lifetime_ms: u64,
}

impl ResumeSealer {
    pub fn new(
        keys: Arc<dyn ResumeKeySource>,
        clock: Arc<dyn Clock>,
        lifetime_ms: u64,
    ) -> Result<Self, ContinuationError> {
        if lifetime_ms == 0 {
            return Err(ContinuationError::InvalidRecord);
        }
        Ok(Self {
            keys,
            clock,
            lifetime_ms,
        })
    }

    pub fn prepare(
        &self,
        authorized: &Authorized,
        floor: ResumeFloor,
    ) -> Result<PreparedResume, ContinuationError> {
        let expires_at = Timestamp::from_millis(
            self.clock
                .now()
                .as_millis()
                .checked_add(self.lifetime_ms)
                .ok_or(ContinuationError::InvalidRecord)?,
        );
        prepare_resume_until(authorized, floor, expires_at, self.keys.as_ref())
    }
}

fn prepare_resume_until(
    authorized: &Authorized,
    floor: ResumeFloor,
    expires_at: Timestamp,
    keys: &dyn ResumeKeySource,
) -> Result<PreparedResume, ContinuationError> {
    let envelope = authorized
        .call()
        .mcp_envelope()
        .ok_or(ContinuationError::AuthorityMismatch)?;
    let original_parameters = envelope.parameters().to_vec();
    let subject = ContinuationSubject::new(
        envelope.subject().server(),
        envelope.operation().method(),
        envelope.admission().generation(),
        envelope.admission().schema(),
    )?;
    let fingerprints = ResumeFingerprints::new(
        envelope.admission().generation(),
        envelope.admission().operation(),
        envelope.admission().schema(),
        envelope.transport_fingerprint(),
    )?;
    let grant = seal_resume(
        authorized,
        subject.clone(),
        fingerprints.clone(),
        floor,
        expires_at,
        keys,
    )?;
    Ok(PreparedResume {
        grant,
        subject,
        fingerprints,
        expires_at,
        original_parameters,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeFingerprints {
    capture: String,
    tool: String,
    schema: String,
    transport: String,
}

impl ResumeFingerprints {
    pub fn new(
        capture: impl Into<String>,
        tool: impl Into<String>,
        schema: impl Into<String>,
        transport: impl Into<String>,
    ) -> Result<Self, ContinuationError> {
        let value = Self {
            capture: capture.into(),
            tool: tool.into(),
            schema: schema.into(),
            transport: transport.into(),
        };
        if [&value.capture, &value.tool, &value.schema, &value.transport]
            .into_iter()
            .any(|part| !valid_value(part))
        {
            return Err(ContinuationError::InvalidGrant);
        }
        Ok(value)
    }

    pub fn capture(&self) -> &str {
        &self.capture
    }
    pub fn tool(&self) -> &str {
        &self.tool
    }
    pub fn schema(&self) -> &str {
        &self.schema
    }
    pub fn transport(&self) -> &str {
        &self.transport
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeFloor {
    workspace: String,
    mode: Mode,
    deny_rule_ids: Vec<String>,
}

impl ResumeFloor {
    pub(crate) fn from_policy(
        workspace: String,
        mode: Mode,
        mut deny_rule_ids: Vec<String>,
    ) -> Self {
        deny_rule_ids.sort();
        deny_rule_ids.dedup();
        Self {
            workspace,
            mode,
            deny_rule_ids,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumePayload {
    call_fingerprint: [u8; 32],
    authority: Authority,
    subject: ContinuationSubject,
    fingerprints: ResumeFingerprints,
    floor: ResumeFloor,
    expires_at: Timestamp,
}

/// Integrity-protected authority retained for one exact resumed operation.
/// Construction is private; durable single use is enforced by the journal.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeGrant {
    payload: Vec<u8>,
    mac: [u8; 32],
}

/// Sealed authority held beside one in-flight MCP operation until MRTR asks.
pub struct PreparedResume {
    grant: ResumeGrant,
    subject: ContinuationSubject,
    fingerprints: ResumeFingerprints,
    expires_at: Timestamp,
    original_parameters: Vec<u8>,
}

impl PreparedResume {
    pub fn into_record(
        self,
        id: impl Into<String>,
        request_state: Vec<u8>,
        correlation_key: impl Into<String>,
        correlation_schema: Vec<u8>,
        transport_fingerprint: impl Into<String>,
    ) -> Result<ContinuationRecord, ContinuationError> {
        let transport_fingerprint = transport_fingerprint.into();
        if transport_fingerprint != self.fingerprints.transport() {
            return Err(ContinuationError::AuthorityMismatch);
        }
        ContinuationRecord::new(
            id,
            self.subject,
            self.grant,
            self.fingerprints,
            self.expires_at,
        )?
        .with_protocol_state(
            self.original_parameters,
            request_state,
            correlation_key,
            correlation_schema,
            transport_fingerprint,
        )
    }
}

impl ResumeGrant {
    pub fn matches_prepared_call_fingerprint(&self, fingerprint: &str) -> bool {
        serde_json::from_slice::<ResumePayload>(&self.payload)
            .is_ok_and(|payload| fingerprint_text(&payload.call_fingerprint) == fingerprint)
    }
}

pub fn seal_resume(
    authorized: &Authorized,
    subject: ContinuationSubject,
    fingerprints: ResumeFingerprints,
    floor: ResumeFloor,
    expires_at: Timestamp,
    keys: &dyn ResumeKeySource,
) -> Result<ResumeGrant, ContinuationError> {
    validate_resume_binding(authorized.call(), &subject, &fingerprints)?;
    if matches!(
        authorized.by(),
        Authority::Boundary | Authority::SessionEvidence
    ) {
        return Err(ContinuationError::AuthorityMismatch);
    }
    let payload = serde_json::to_vec(&ResumePayload {
        call_fingerprint: authorized.call().authority_fingerprint(),
        authority: authorized.by().clone(),
        subject,
        fingerprints,
        floor,
        expires_at,
    })
    .map_err(|_| ContinuationError::InvalidGrant)?;
    let key = keys.resume_key()?;
    validate_key(&key)?;
    let mac = hmac_sha256(&key, &payload);
    Ok(ResumeGrant { payload, mac })
}

#[allow(clippy::too_many_arguments)]
pub fn open_resume(
    grant: ResumeGrant,
    reconstructed_call: PreparedCall,
    subject: &ContinuationSubject,
    fingerprints: &ResumeFingerprints,
    current_floor: &ResumeFloor,
    now: Timestamp,
    keys: &dyn ResumeKeySource,
) -> Result<Authorized, ContinuationError> {
    let key = keys.resume_key()?;
    validate_key(&key)?;
    let expected = hmac_sha256(&key, &grant.payload);
    if !constant_time_eq(&grant.mac, &expected) {
        return Err(ContinuationError::InvalidGrant);
    }
    let payload: ResumePayload =
        serde_json::from_slice(&grant.payload).map_err(|_| ContinuationError::InvalidGrant)?;
    if now >= payload.expires_at
        || &payload.subject != subject
        || &payload.fingerprints != fingerprints
        || &payload.floor != current_floor
        || payload.call_fingerprint != reconstructed_call.authority_fingerprint()
    {
        return Err(if now >= payload.expires_at {
            ContinuationError::Expired
        } else {
            ContinuationError::AuthorityMismatch
        });
    }
    validate_resume_binding(&reconstructed_call, subject, fingerprints)?;
    Ok(Authorized::from_verified_resume(
        reconstructed_call,
        payload.authority,
    ))
}

fn validate_resume_binding(
    call: &PreparedCall,
    subject: &ContinuationSubject,
    fingerprints: &ResumeFingerprints,
) -> Result<(), ContinuationError> {
    let envelope = call
        .mcp_envelope()
        .ok_or(ContinuationError::AuthorityMismatch)?;
    if envelope.subject().server() != subject.server()
        || envelope.operation().method() != subject.operation()
        || envelope.admission().generation() != fingerprints.capture()
        || envelope.admission().operation() != fingerprints.tool()
        || envelope.admission().schema() != fingerprints.schema()
        || envelope.transport_fingerprint() != fingerprints.transport()
        || subject.admission_generation() != fingerprints.capture()
        || subject.descriptor_fingerprint() != fingerprints.schema()
    {
        return Err(ContinuationError::AuthorityMismatch);
    }
    Ok(())
}

fn validate_key(key: &[u8]) -> Result<(), ContinuationError> {
    if key.len() < 32 {
        Err(ContinuationError::InvalidKey)
    } else {
        Ok(())
    }
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut normalized = [0_u8; BLOCK];
    if key.len() > BLOCK {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK];
    let mut outer_pad = [0x5c_u8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn fingerprint_text(fingerprint: &[u8; 32]) -> String {
    let mut text = String::with_capacity(71);
    text.push_str("sha256:");
    for byte in fingerprint {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

impl fmt::Debug for ResumeGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResumeGrant")
            .field("prepared_call_fingerprint", &"<redacted>")
            .field("sealed", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationState {
    Pending,
    Accepted,
    Claimed,
    Completed,
    Failed,
    Uncertain,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationRecord {
    id: String,
    subject: ContinuationSubject,
    grant: ResumeGrant,
    fingerprints: ResumeFingerprints,
    original_parameters: Vec<u8>,
    request_state: Vec<u8>,
    correlation_key: String,
    correlation_schema: Vec<u8>,
    transport_fingerprint: Option<String>,
    accepted_answer: Option<Vec<u8>>,
    expires_at: Timestamp,
    state: ContinuationState,
}

impl ContinuationRecord {
    pub fn new(
        id: impl Into<String>,
        subject: ContinuationSubject,
        grant: ResumeGrant,
        fingerprints: ResumeFingerprints,
        expires_at: Timestamp,
    ) -> Result<Self, ContinuationError> {
        let id = id.into();
        if !valid_value(&id) {
            return Err(ContinuationError::InvalidId);
        }
        Ok(Self {
            id,
            subject,
            grant,
            fingerprints,
            original_parameters: Vec::new(),
            request_state: Vec::new(),
            correlation_key: String::new(),
            correlation_schema: Vec::new(),
            transport_fingerprint: None,
            accepted_answer: None,
            expires_at,
            state: ContinuationState::Pending,
        })
    }

    pub fn with_protocol_state(
        mut self,
        original_parameters: Vec<u8>,
        request_state: Vec<u8>,
        correlation_key: impl Into<String>,
        correlation_schema: Vec<u8>,
        transport_fingerprint: impl Into<String>,
    ) -> Result<Self, ContinuationError> {
        let transport_fingerprint = transport_fingerprint.into();
        let correlation_key = correlation_key.into();
        if original_parameters.is_empty()
            || request_state.is_empty()
            || !valid_value(&correlation_key)
            || correlation_schema.is_empty()
            || !valid_value(&transport_fingerprint)
        {
            return Err(ContinuationError::InvalidRecord);
        }
        self.original_parameters = original_parameters;
        self.request_state = request_state;
        self.correlation_key = correlation_key;
        self.correlation_schema = correlation_schema;
        self.transport_fingerprint = Some(transport_fingerprint);
        Ok(self)
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn subject(&self) -> &ContinuationSubject {
        &self.subject
    }
    pub fn admission_generation(&self) -> &str {
        self.subject.admission_generation()
    }
    pub fn descriptor_fingerprint(&self) -> &str {
        self.subject.descriptor_fingerprint()
    }
    pub fn fingerprints(&self) -> &ResumeFingerprints {
        &self.fingerprints
    }
    pub fn matches_prepared_call_fingerprint(&self, fingerprint: &str) -> bool {
        self.grant.matches_prepared_call_fingerprint(fingerprint)
    }
    pub fn transport_fingerprint(&self) -> Option<&str> {
        self.transport_fingerprint.as_deref()
    }
    pub fn correlation_schema(&self) -> &[u8] {
        &self.correlation_schema
    }
    pub fn expires_at(&self) -> Timestamp {
        self.expires_at
    }
    pub fn state(&self) -> ContinuationState {
        self.state
    }

    pub fn into_accepted(self) -> Result<AcceptedContinuation, ContinuationError> {
        if self.state != ContinuationState::Accepted
            || self.accepted_answer.is_none()
            || self.original_parameters.is_empty()
            || self.request_state.is_empty()
            || self.correlation_key.is_empty()
            || self.correlation_schema.is_empty()
            || self.transport_fingerprint.is_none()
        {
            return Err(ContinuationError::InvalidTransition);
        }
        Ok(AcceptedContinuation {
            id: self.id,
            subject: self.subject,
            grant: self.grant,
            fingerprints: self.fingerprints,
            original_parameters: self.original_parameters,
            request_state: self.request_state,
            correlation_key: self.correlation_key,
            correlation_schema: self.correlation_schema,
            transport_fingerprint: self
                .transport_fingerprint
                .expect("accepted protocol state was checked"),
            accepted_answer: self.accepted_answer.expect("accepted answer was checked"),
            expires_at: self.expires_at,
        })
    }

    pub fn validate_fingerprints(
        &self,
        admission_generation: &str,
        descriptor_fingerprint: &str,
        prepared_call_fingerprint: &str,
    ) -> Result<(), ContinuationError> {
        if self.subject.admission_generation() != admission_generation
            || self.subject.descriptor_fingerprint() != descriptor_fingerprint
            || !self
                .grant
                .matches_prepared_call_fingerprint(prepared_call_fingerprint)
        {
            return Err(ContinuationError::AuthorityMismatch);
        }
        Ok(())
    }

    pub fn accept_answer(
        &mut self,
        answer: Vec<u8>,
        now: Timestamp,
    ) -> Result<(), ContinuationError> {
        if now >= self.expires_at {
            return Err(ContinuationError::Expired);
        }
        if self.state != ContinuationState::Pending || self.accepted_answer.is_some() {
            return Err(ContinuationError::DuplicateAnswer);
        }
        if answer.is_empty() {
            return Err(ContinuationError::InvalidAnswer);
        }
        self.accepted_answer = Some(answer);
        self.state = ContinuationState::Accepted;
        Ok(())
    }

    pub fn claim(mut self, now: Timestamp) -> Result<ClaimedContinuation, ContinuationError> {
        if now >= self.expires_at {
            return Err(ContinuationError::Expired);
        }
        match self.state {
            ContinuationState::Accepted => {
                self.state = ContinuationState::Claimed;
                Ok(ClaimedContinuation { record: self })
            }
            ContinuationState::Claimed
            | ContinuationState::Completed
            | ContinuationState::Failed
            | ContinuationState::Uncertain => Err(ContinuationError::DuplicateClaim),
            ContinuationState::Pending => Err(ContinuationError::AnswerRequired),
        }
    }

    pub fn settle(&mut self, state: ContinuationState) -> Result<(), ContinuationError> {
        if self.state != ContinuationState::Claimed
            || !matches!(
                state,
                ContinuationState::Completed
                    | ContinuationState::Failed
                    | ContinuationState::Uncertain
            )
        {
            return Err(ContinuationError::InvalidTransition);
        }
        self.state = state;
        Ok(())
    }
}

/// Durable accepted answer awaiting fresh admission and one atomic claim.
pub struct AcceptedContinuation {
    id: String,
    subject: ContinuationSubject,
    grant: ResumeGrant,
    fingerprints: ResumeFingerprints,
    original_parameters: Vec<u8>,
    request_state: Vec<u8>,
    correlation_key: String,
    correlation_schema: Vec<u8>,
    transport_fingerprint: String,
    accepted_answer: Vec<u8>,
    expires_at: Timestamp,
}

impl AcceptedContinuation {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn subject(&self) -> &ContinuationSubject {
        &self.subject
    }

    pub fn fingerprints(&self) -> &ResumeFingerprints {
        &self.fingerprints
    }

    pub fn original_parameters(&self) -> &[u8] {
        &self.original_parameters
    }

    pub fn correlation_schema(&self) -> &[u8] {
        &self.correlation_schema
    }

    pub fn accepted_answer(&self) -> &[u8] {
        &self.accepted_answer
    }

    pub fn open(
        self,
        reconstructed_call: PreparedCall,
        current_fingerprints: &ResumeFingerprints,
        current_floor: &ResumeFloor,
        now: Timestamp,
        keys: &dyn ResumeKeySource,
    ) -> Result<OpenedContinuation, ContinuationError> {
        if current_fingerprints != &self.fingerprints
            || current_fingerprints.transport() != self.transport_fingerprint
        {
            return Err(ContinuationError::AuthorityMismatch);
        }
        let authorized = open_resume(
            self.grant,
            reconstructed_call,
            &self.subject,
            current_fingerprints,
            current_floor,
            now,
            keys,
        )?;
        Ok(OpenedContinuation {
            id: self.id,
            authorized,
            original_parameters: self.original_parameters,
            request_state: self.request_state,
            correlation_key: self.correlation_key,
            correlation_schema: self.correlation_schema,
            accepted_answer: self.accepted_answer,
            expires_at: self.expires_at,
        })
    }
}

/// Verified original authority and correlated data for one claimed retry.
pub struct OpenedContinuation {
    id: String,
    authorized: Authorized,
    original_parameters: Vec<u8>,
    request_state: Vec<u8>,
    correlation_key: String,
    correlation_schema: Vec<u8>,
    accepted_answer: Vec<u8>,
    expires_at: Timestamp,
}

impl OpenedContinuation {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn authorized(&self) -> &Authorized {
        &self.authorized
    }

    pub fn original_parameters(&self) -> &[u8] {
        &self.original_parameters
    }

    pub fn request_state(&self) -> &[u8] {
        &self.request_state
    }

    pub fn correlation_schema(&self) -> &[u8] {
        &self.correlation_schema
    }

    pub fn correlation_key(&self) -> &str {
        &self.correlation_key
    }

    pub fn accepted_answer(&self) -> &[u8] {
        &self.accepted_answer
    }

    pub fn expires_at(&self) -> Timestamp {
        self.expires_at
    }

    /// Call only after the protocol owner has validated the stored answer
    /// against the stored correlation schema.
    pub fn into_retry_authorized(self) -> Result<Authorized, ContinuationError> {
        let mut parameters: serde_json::Value = serde_json::from_slice(&self.original_parameters)
            .map_err(|_| ContinuationError::InvalidRecord)?;
        let parameters = parameters
            .as_object_mut()
            .ok_or(ContinuationError::InvalidRecord)?;
        let answer = serde_json::from_slice(&self.accepted_answer)
            .map_err(|_| ContinuationError::InvalidAnswer)?;
        let request_state =
            String::from_utf8(self.request_state).map_err(|_| ContinuationError::InvalidRecord)?;
        parameters.insert(
            "inputResponses".to_owned(),
            serde_json::Value::Object(serde_json::Map::from_iter([(self.correlation_key, answer)])),
        );
        parameters.insert(
            "requestState".to_owned(),
            serde_json::Value::String(request_state),
        );
        let parameters = serde_json::Value::Object(parameters.clone());
        self.authorized
            .with_verified_resume_parameters(&parameters)
            .map_err(|_| ContinuationError::InvalidRecord)
    }

    /// Consume one validated retry while preparing authority for a possible
    /// successor round. The successor can never outlive its predecessor.
    pub fn into_retry_authorized_with_successor(
        self,
        floor: ResumeFloor,
        now: Timestamp,
        keys: &dyn ResumeKeySource,
    ) -> Result<(Authorized, PreparedResume), ContinuationError> {
        if now >= self.expires_at {
            return Err(ContinuationError::Expired);
        }
        let expires_at = self.expires_at;
        let successor = prepare_resume_until(&self.authorized, floor, expires_at, keys)?;
        let authorized = self.into_retry_authorized()?;
        Ok((authorized, successor))
    }
}

/// Non-forgeable, non-cloneable proof of one durable claim.
pub struct ClaimedContinuation {
    record: ContinuationRecord,
}

impl ClaimedContinuation {
    pub fn id(&self) -> &str {
        self.record.id()
    }

    pub fn record(&self) -> &ContinuationRecord {
        &self.record
    }

    pub fn settle(
        mut self,
        state: ContinuationState,
    ) -> Result<ContinuationRecord, ContinuationError> {
        self.record.settle(state)?;
        Ok(self.record)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ContinuationEvent {
    Recorded {
        record: ContinuationRecordEvent,
    },
    AnswerAccepted {
        id: String,
        answer: Vec<u8>,
    },
    Claimed {
        id: String,
        at: Timestamp,
    },
    Settled {
        id: String,
        state: ContinuationState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationRecordEvent {
    pub id: String,
    pub subject: ContinuationSubject,
    pub expires_at: Timestamp,
}

impl From<&ContinuationRecord> for ContinuationRecordEvent {
    fn from(record: &ContinuationRecord) -> Self {
        Self {
            id: record.id.clone(),
            subject: record.subject.clone(),
            expires_at: record.expires_at,
        }
    }
}

/// Persistence boundary for durable, atomic continuation transitions.
pub trait ContinuationJournal: Send + Sync {
    fn record(&self, record: ContinuationRecord) -> Result<(), ContinuationError>;
    fn accept_answer(
        &self,
        id: &str,
        answer: &[u8],
        now: Timestamp,
    ) -> Result<(), ContinuationError>;
    fn load(&self, id: &str) -> Result<ContinuationRecord, ContinuationError>;
    fn claim(&self, id: &str, now: Timestamp) -> Result<ClaimedContinuation, ContinuationError>;
    fn settle(
        &self,
        claimed: ClaimedContinuation,
        state: ContinuationState,
    ) -> Result<(), ContinuationError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuationError {
    InvalidId,
    InvalidSubject,
    UnsupportedSubject(String),
    InvalidGrant,
    InvalidKey,
    InvalidRecord,
    InvalidAnswer,
    DuplicateAnswer,
    DuplicateClaim,
    AnswerRequired,
    AuthorityMismatch,
    Expired,
    NotFound,
    InvalidTransition,
    Corrupt(String),
    Unavailable(String),
}

pub type JournalError = ContinuationError;

impl ContinuationError {
    pub fn is_expired(&self) -> bool {
        matches!(self, Self::Expired)
    }
}

impl fmt::Display for ContinuationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId => formatter.write_str("invalid continuation identifier"),
            Self::InvalidSubject => formatter.write_str("invalid continuation subject"),
            Self::UnsupportedSubject(operation) => {
                write!(formatter, "unsupported continuation subject: {operation}")
            }
            Self::InvalidGrant => formatter.write_str("invalid sealed resume grant"),
            Self::InvalidKey => formatter.write_str("invalid resume sealing key"),
            Self::InvalidRecord => formatter.write_str("invalid continuation record"),
            Self::InvalidAnswer => formatter.write_str("invalid continuation answer"),
            Self::DuplicateAnswer => {
                formatter.write_str("continuation answer was already accepted")
            }
            Self::DuplicateClaim => formatter.write_str("continuation was already claimed"),
            Self::AnswerRequired => formatter.write_str("continuation answer is required"),
            Self::AuthorityMismatch => formatter.write_str("continuation authority mismatch"),
            Self::Expired => formatter.write_str("continuation expired"),
            Self::NotFound => formatter.write_str("continuation not found"),
            Self::InvalidTransition => formatter.write_str("invalid continuation state transition"),
            Self::Corrupt(message) => write!(formatter, "corrupt continuation journal: {message}"),
            Self::Unavailable(message) => {
                write!(formatter, "continuation journal unavailable: {message}")
            }
        }
    }
}

impl std::error::Error for ContinuationError {}

fn valid_value(value: &str) -> bool {
    !value.trim().is_empty() && !value.chars().any(char::is_control)
}
