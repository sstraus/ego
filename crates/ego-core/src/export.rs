//! Producer-only structural export of durable session facts.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::Serialize;

use crate::budget::MeteringIdentity;
use crate::ceiling::Ceilings;
use crate::config::ModelRef;
use crate::cost::{CostDelta, PricingRevision};
use crate::event::domain::{
    EventKind, ExitReason, FallbackDecision, GenerationRole, OccupancyProvenance, SubagentTerminal,
};
use crate::event::hook::{
    HookActivation, HookContentDigest, HookDecision, HookFailureKind, HookId, HookInvocationId,
    HookLifecycle, HookOccurrenceKey, HookPlanEntry, HookResult, HookSourceAuthority, HookSubject,
};
use crate::event::id::{EventId, ItemId, Producer, QueuedPromptId, Seq, SessionId, TurnId};
use crate::event::time::Timestamp;
use crate::evidence::{
    ConfigProvenance, EVIDENCE_SCHEMA_VERSION, EgoVersion, EvidenceDigest, EvidenceKey,
    EvidenceRecord, ExecutionAdmissionDescriptor, GenerationTarget, McpContextDescriptor,
    PricingEvidence, RunInvocationId,
};
use crate::generator::attempt::{AttemptRef, Outcome};
use crate::generator::capabilities::ModelCapabilities;
use crate::generator::types::{ErrorKind, ImageRef};
use crate::permit::residual::{ResidualOutcome, ResidualScope};
use crate::permit::{Escalation, ExecObservation, PolicyDigest};
use crate::public::InstructionSource;
use crate::question::{QuestionMode, QuestionOutcome, QuestionTarget};
use crate::run::{
    CancelReason, RequestDigest, RequestId, RunCommand, RunId, RunRevision, RunSelections,
    RunSettlement, RunStatus, TurnLease,
};
use crate::session::{
    LineageEdges, ReplayError, ReplaySource, ReplaySourceError, SessionBlobSource, SessionLineage,
    map_source, read_verified_blob, walk_lineage,
};
use crate::state::Usage;
use crate::store::blob::BlobRef;
use crate::store::session_store::SessionEntry;

pub const EXPORT_BUNDLE_VERSION: u32 = 1;

pub trait ExportSource: ReplaySource + SessionBlobSource {}

impl<T: ReplaySource + SessionBlobSource> ExportSource for T {}

/// One complete producer-only sanitized disclosure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportBundle {
    pub v: u32,
    pub target: SessionId,
    pub records: Vec<ExportRecord>,
    pub redactions: Vec<RedactionCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportRecord {
    ToolCatalogueEvidence {
        session: SessionId,
        position: Seq,
        digest: EvidenceDigest,
        tools: usize,
    },
    Session {
        session: SessionId,
        lineage: Option<SessionLineage>,
    },
    Event {
        session: SessionId,
        position: Seq,
        event_seq: Seq,
        event_id: EventId,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
        observed_at: Timestamp,
        producer: Producer,
        event: ExportEvent,
    },
    RunTransaction {
        session: SessionId,
        position: Seq,
        revision: RunRevision,
        run_id: RunId,
        selections: RunSelections,
        status: RunStatus,
        observed_at: Timestamp,
        pruned_receipts: Vec<TurnLease>,
        command: ExportRunCommand,
    },
    ModelSelected {
        session: SessionId,
        position: Seq,
        model: ModelRef,
    },
    NoticeDelivered {
        session: SessionId,
        position: Seq,
        card_id: String,
        delivered_at: Timestamp,
        source_path_present: bool,
    },
    RunContextEvidence {
        evidence_v: u32,
        session: SessionId,
        position: Seq,
        invocation: RunInvocationId,
        run_id: RunId,
        record_digest: String,
        ego_version: EgoVersion,
        config: ConfigProvenance,
        config_digest: EvidenceDigest,
        policy_digest: PolicyDigest,
        ceilings: Ceilings,
        instructions: Vec<ExportInstructionEvidence>,
        mcp: Vec<McpContextDescriptor>,
        execution: ExecutionAdmissionDescriptor,
    },
    GenerationAdmissionEvidence {
        evidence_v: u32,
        session: SessionId,
        position: Seq,
        invocation: RunInvocationId,
        attempt: AttemptRef,
        role: GenerationRole,
        record_digest: String,
        target: GenerationTarget,
        capabilities: ModelCapabilities,
        pricing: ExportPricingEvidence,
    },
    BlobReference {
        session: SessionId,
        event: EventId,
        reference: BlobRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportInstructionEvidence {
    pub sha256: String,
    pub bytes: u64,
    pub source: InstructionSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportPricingEvidence {
    Priced {
        source: String,
        revision: PricingRevision,
        metering: MeteringIdentity,
    },
    Unpriced {
        source: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportRunCommand {
    CommandAdmitted {
        request_id: RequestId,
        digest: RequestDigest,
        expected_revision: Option<RunRevision>,
        lease: TurnLease,
        executor_id: uuid::Uuid,
        model: ModelRef,
        start: bool,
    },
    CancelRequested {
        request_id: RequestId,
        digest: RequestDigest,
        lease: TurnLease,
    },
    CommandSettled {
        lease: TurnLease,
        settlement: ExportRunSettlement,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExportRunSettlement {
    Completed,
    ContextOverflow,
    Paused,
    Cancelled {
        reason: CancelReason,
    },
    PermissionRequired,
    /// Exported distinctly from [`Interrupted`] rather than folded into
    /// `Cancelled`: an export is evidence, and evidence that cannot say
    /// whether a cancellation beat the executor or an invariant broke is the
    /// ambiguity this story exists to remove.
    CancelledBeforeStart,
    Interrupted,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportEvent {
    SessionTitleChanged,
    SessionDeleted,
    /// A pause was asked for. Exported with its request id and no redaction
    /// count: the id is one the host minted for correlation and carries no
    /// operator prose, and a session that stopped needs the record of *why*
    /// it stopped or an audit reads a deliberate hold as a crash.
    SessionPauseRequested {
        request: RequestId,
    },
    SessionPaused {
        request: RequestId,
    },
    /// A request was told about a hold it did not establish. Exported for the
    /// same reason as the pair: without it an audit sees a request answered
    /// `paused` and no record it was ever asked, and later sees that same id
    /// replayed with nothing to replay from.
    SessionPauseObserved {
        request: RequestId,
    },
    SessionResumed {
        request: RequestId,
    },
    GoalSet,
    GoalTransitioned,
    GoalContinuation,
    ContextRequested,
    ContextCooldown,
    ContextRequestConsumed,
    GoalCheckConfigured,
    GoalAllowanceInherited,
    GoalUpdated,
    GoalCheckStopped,
    PromptQueued {
        id: QueuedPromptId,
        images: Vec<ImageRef>,
    },
    PromptDeliveryExpedited {
        through: QueuedPromptId,
    },
    PromptRetracted {
        id: QueuedPromptId,
    },
    UserPrompt {
        images: Vec<ImageRef>,
        queued_prompt: Option<QueuedPromptId>,
    },
    AssistantDelta,
    AssistantMessage,
    HostContextAppended,
    ReasoningDelta,
    ReasoningState,
    ToolCallStarted,
    ToolCallDelta,
    ToolCallCompleted {
        ok: bool,
    },
    ToolCallMeasured {
        timing: crate::event::domain::ToolTiming,
    },
    DetachedExecStarted {
        id: crate::event::id::DetachedExecId,
    },
    DetachedExecSettled {
        id: crate::event::id::DetachedExecId,
        state: String,
        exit_code: Option<i32>,
        stdout_omitted: u64,
        stderr_omitted: u64,
    },
    QuestionRequested {
        target: QuestionTarget,
        mode: QuestionMode,
        option_count: u64,
        free_text: bool,
    },
    QuestionSettled {
        outcome: ExportQuestionOutcome,
    },
    PermissionRequested,
    PermissionAmended,
    PermissionDecided {
        allowed: Option<bool>,
    },
    GenerationStarted {
        attempt: AttemptRef,
        role: GenerationRole,
    },
    FallbackDecision {
        candidate: ModelRef,
        decision: ExportFallbackDecision,
    },
    ExecutionObserved {
        observation: ExecObservation,
        probe_evidence: Option<crate::store::evidence::EvidenceRef>,
    },
    ExecutionMeasured {
        observation: ExecObservation,
        probe_evidence: Option<crate::store::evidence::EvidenceRef>,
    },
    JudgeResponse {
        attempt: AttemptRef,
        bytes: usize,
        truncated: bool,
    },
    SandboxResidualDecided {
        outcome: ResidualOutcome,
        recommended: Option<ResidualScope>,
        attempt: Option<AttemptRef>,
    },
    AttemptSettled {
        attempt: AttemptRef,
        outcome: Outcome,
    },
    UsageUpdated {
        usage: Usage,
        cost_delta: CostDelta,
        attempt: Option<AttemptRef>,
    },
    UsageCorrected {
        attempt: AttemptRef,
        usage: Usage,
        cost_delta: CostDelta,
    },
    ContextWindowUpdated {
        used_tokens: u64,
        provenance: OccupancyProvenance,
        /// The ceiling `used_tokens` is a fraction of, when the reporting
        /// generation stated one.
        ///
        /// Exported because an occupancy without its denominator is not an
        /// auditable fact: a reader would have to go back to the model
        /// catalogue as it stood at the time, which for a turn that fell back
        /// to another model is not recoverable from this file at all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<u64>,
    },
    TurnCompleted {
        ok: bool,
        exit_reason: ExitReason,
    },
    CompactionSeeded {
        source: SessionId,
        source_seq: Seq,
        prompt_revision: u32,
    },
    Compacted,
    Forked {
        source: SessionId,
        at_seq: Seq,
    },
    HookCatalogueAdmitted {
        descriptors: Vec<ExportHookDescriptor>,
    },
    HookLifecycleReached {
        occurrence: HookOccurrenceKey,
        subject: HookSubject,
        plan: Vec<HookPlanEntry>,
    },
    HookInvocationStarted {
        occurrence: EventId,
        hook: HookId,
        invocation: HookInvocationId,
        lifecycle: HookLifecycle,
        subject: HookSubject,
        activation: HookActivation,
    },
    HookInvocationSettled {
        occurrence: EventId,
        hook: HookId,
        invocation: HookInvocationId,
        result: ExportHookResult,
    },
    SubagentRequested,
    SubagentSpawned,
    SubagentSettled,
    SkillCatalogueAdmitted,
    SkillInstructionAdmitted,
    SkillResourceAdmitted,
    Error {
        error_kind: ErrorKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportQuestionOutcome {
    Answered,
    Declined,
    Cancelled,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExportFallbackDecision {
    ReasoningDropped {
        dropped_blocks: usize,
        dialects: Vec<String>,
    },
    Skipped {
        gaps: Vec<crate::generator::capabilities::CapabilityGap>,
    },
    ExpressibilitySkipped,
    ResolutionSkipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportHookResult {
    Outcome { decision: HookDecision },
    Failure { failure: HookFailureKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ExportHookDescriptor {
    pub id: HookId,
    pub activation: HookActivation,
    pub source: HookSourceAuthority,
    pub digest: HookContentDigest,
    pub ordinal: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RedactionCount {
    pub category: RedactionCategory,
    pub count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionCategory {
    OpaqueProse,
    HiddenReasoning,
    Path,
    ToolMaterial,
    PermissionMaterial,
    RawError,
    BlobPayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    Replay(ReplayError),
    Evidence {
        session: SessionId,
        position: Seq,
        failure: ExportEvidenceFailure,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportEvidenceFailure {
    UnsupportedVersion {
        found: u32,
        required: u32,
    },
    MissingRunEvidence,
    MissingRunContext {
        invocation: RunInvocationId,
    },
    MissingGenerationAdmission {
        attempt: AttemptRef,
    },
    Duplicate {
        key: EvidenceKey,
    },
    OutOfOrder {
        key: EvidenceKey,
    },
    SessionMismatch {
        recorded: SessionId,
    },
    RunMismatch {
        invocation: RunInvocationId,
    },
    GenerationRoleMismatch {
        attempt: AttemptRef,
    },
    AttemptOwnershipConflict {
        attempt: AttemptRef,
        existing_invocation: RunInvocationId,
        requested_invocation: RunInvocationId,
    },
}

impl fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Replay(error) => error.fmt(formatter),
            Self::Evidence {
                session,
                position,
                failure,
            } => write!(
                formatter,
                "session {session} has invalid run evidence at {position:?}: {failure:?}"
            ),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<ReplayError> for ExportError {
    fn from(error: ReplayError) -> Self {
        Self::Replay(error)
    }
}

pub struct Exporter<'a> {
    source: &'a dyn ExportSource,
}

impl<'a> Exporter<'a> {
    pub fn new(source: &'a dyn ExportSource) -> Self {
        Self { source }
    }

    pub fn sanitized(&self, target: SessionId) -> Result<ExportBundle, ExportError> {
        let target_lineage = self.source.lineage(&target).map_err(source_error)?;
        let mut nodes = vec![(target, target_lineage)];
        if let Some(lineage) = target_lineage {
            let mut visited = HashSet::from([target]);
            nodes.extend(
                walk_lineage(
                    self.source,
                    lineage,
                    &mut visited,
                    LineageEdges::ForksAndCompactions,
                )?
                .into_iter()
                .map(|step| (step.source, step.lineage)),
            );
        }
        nodes.reverse();

        let mut records = Vec::new();
        let mut redactions = Redactions::default();
        for (session, lineage) in nodes {
            let entries = self
                .source
                .entries(&session)
                .map_err(source_error)?
                .ok_or(ReplayError::MissingSession { session })?;
            records.push(ExportRecord::Session { session, lineage });
            self.project_session(session, entries, &mut records, &mut redactions)?;
        }

        Ok(ExportBundle {
            v: EXPORT_BUNDLE_VERSION,
            target,
            records,
            redactions: redactions.rows(),
        })
    }

    fn project_session(
        &self,
        session: SessionId,
        entries: Vec<SessionEntry>,
        records: &mut Vec<ExportRecord>,
        redactions: &mut Redactions,
    ) -> Result<(), ExportError> {
        let mut evidence = EvidenceValidation::default();
        let mut verified_blobs = HashSet::new();
        let mut position = Seq::ZERO;
        for entry in entries {
            let position = position.next();
            match entry {
                SessionEntry::Event(event) => {
                    evidence.event(session, position, &event.kind)?;
                    let references = event.kind.blob_references();
                    let projected = project_event(&event.kind, redactions);
                    records.push(ExportRecord::Event {
                        session,
                        position,
                        event_seq: event.env.seq,
                        event_id: event.env.id,
                        turn: event.env.turn,
                        item: event.env.item,
                        parent: event.env.parent,
                        observed_at: event.env.ts,
                        producer: event.env.producer,
                        event: projected,
                    });
                    for reference in references {
                        if !verified_blobs.insert(reference.clone()) {
                            continue;
                        }
                        read_verified_blob(self.source, session, &reference)?;
                        redactions.blob_payload += 1;
                        records.push(ExportRecord::BlobReference {
                            session,
                            event: event.env.id,
                            reference,
                        });
                    }
                }
                SessionEntry::RunTransaction(transaction) => {
                    evidence.transaction(session, position, &transaction)?;
                    let command = project_run_command(transaction.command(), redactions);
                    records.push(ExportRecord::RunTransaction {
                        session,
                        position,
                        revision: transaction.revision(),
                        run_id: transaction.run_id(),
                        selections: transaction.selections().clone(),
                        status: transaction.status(),
                        observed_at: transaction.observed_at(),
                        pruned_receipts: transaction.pruned_receipts().to_vec(),
                        command,
                    });
                }
                SessionEntry::ModelSelected(model) => {
                    records.push(ExportRecord::ModelSelected {
                        session,
                        position,
                        model,
                    });
                }
                SessionEntry::NoticeDelivered(notice) => {
                    records.push(ExportRecord::NoticeDelivered {
                        session,
                        position,
                        card_id: notice.card_id,
                        delivered_at: notice.delivered_at,
                        source_path_present: notice.source_path.is_some(),
                    });
                }
                SessionEntry::Evidence(record) => {
                    evidence.record(session, position, &record)?;
                    records.push(project_evidence(session, position, &record, redactions));
                }
            }
        }
        evidence.finish(session, position)
    }
}

fn source_error(error: ReplaySourceError) -> ExportError {
    ExportError::Replay(map_source(error))
}

fn project_evidence(
    session: SessionId,
    position: Seq,
    record: &EvidenceRecord,
    redactions: &mut Redactions,
) -> ExportRecord {
    match record {
        EvidenceRecord::ToolCatalogue(catalogue) => {
            redactions.opaque_prose += catalogue.tools.len() as u64;
            ExportRecord::ToolCatalogueEvidence {
                session,
                position,
                digest: catalogue.digest.clone(),
                tools: catalogue.tools.len(),
            }
        }
        EvidenceRecord::RunContext(context) => {
            let instructions = context
                .instructions()
                .iter()
                .map(|instruction| {
                    redactions.path += 2;
                    ExportInstructionEvidence {
                        sha256: instruction.sha256.clone(),
                        bytes: instruction.bytes,
                        source: instruction.source,
                    }
                })
                .collect();
            ExportRecord::RunContextEvidence {
                evidence_v: context.version(),
                session,
                position,
                invocation: context.invocation(),
                run_id: context.run_id(),
                record_digest: record.digest().to_string(),
                ego_version: context.ego_version().clone(),
                config: context.config().clone(),
                config_digest: context.config_digest().clone(),
                policy_digest: context.policy_digest().clone(),
                ceilings: context.ceilings().clone(),
                instructions,
                mcp: context.mcp().to_vec(),
                execution: context.execution().clone(),
            }
        }
        EvidenceRecord::GenerationAdmission(admission) => {
            let pricing = match admission.pricing() {
                PricingEvidence::Priced {
                    source,
                    revision,
                    metering,
                } => ExportPricingEvidence::Priced {
                    source: source.clone(),
                    revision: revision.clone(),
                    metering: metering.clone(),
                },
                PricingEvidence::Unpriced { source, reason: _ } => {
                    redactions.opaque_prose += 1;
                    ExportPricingEvidence::Unpriced {
                        source: source.clone(),
                    }
                }
            };
            ExportRecord::GenerationAdmissionEvidence {
                evidence_v: admission.version(),
                session,
                position,
                invocation: admission.invocation(),
                attempt: admission.attempt(),
                role: admission.role(),
                record_digest: record.digest().to_string(),
                target: admission.target().clone(),
                capabilities: admission.capabilities().clone(),
                pricing,
            }
        }
    }
}

fn project_run_command(command: &RunCommand, redactions: &mut Redactions) -> ExportRunCommand {
    match command {
        RunCommand::CommandAdmitted {
            request_id,
            digest,
            expected_revision,
            lease,
            executor_id,
            prompt: _,
            model,
            start,
        } => {
            redactions.opaque_prose += 1;
            ExportRunCommand::CommandAdmitted {
                request_id: *request_id,
                digest: digest.clone(),
                expected_revision: *expected_revision,
                lease: *lease,
                executor_id: *executor_id,
                model: model.clone(),
                start: *start,
            }
        }
        RunCommand::CancelRequested {
            request_id,
            digest,
            lease,
        } => ExportRunCommand::CancelRequested {
            request_id: *request_id,
            digest: digest.clone(),
            lease: *lease,
        },
        RunCommand::CommandSettled { lease, settlement } => {
            let settlement = match settlement {
                RunSettlement::Completed => ExportRunSettlement::Completed,
                RunSettlement::ContextOverflow => ExportRunSettlement::ContextOverflow,
                RunSettlement::Paused => ExportRunSettlement::Paused,
                RunSettlement::Cancelled { reason } => {
                    ExportRunSettlement::Cancelled { reason: *reason }
                }
                RunSettlement::PermissionRequired { action: _, by: _ } => {
                    redactions.permission_material += 1;
                    ExportRunSettlement::PermissionRequired
                }
                RunSettlement::CancelledBeforeStart => ExportRunSettlement::CancelledBeforeStart,
                // The diagnostic is sanitized for the host that asked, not for
                // an export that leaves this machine: a `RunError` display can
                // still name a path or an executor's own words. The failure
                // kind survives; the sentence explaining it is redacted like
                // any other raw error.
                RunSettlement::Interrupted { diagnostic } => {
                    redactions.raw_error += u64::from(diagnostic.is_some());
                    ExportRunSettlement::Interrupted
                }
                RunSettlement::Uncertain { diagnostic } => {
                    redactions.raw_error += u64::from(diagnostic.is_some());
                    ExportRunSettlement::Uncertain
                }
            };
            ExportRunCommand::CommandSettled {
                lease: *lease,
                settlement,
            }
        }
    }
}

fn project_event(kind: &EventKind, redactions: &mut Redactions) -> ExportEvent {
    match kind {
        EventKind::SessionTitleChanged { title } => {
            redactions.opaque_prose += title.is_some() as u64;
            ExportEvent::SessionTitleChanged
        }
        EventKind::SessionDeleted => ExportEvent::SessionDeleted,
        EventKind::SessionPauseRequested { request } => {
            ExportEvent::SessionPauseRequested { request: *request }
        }
        EventKind::SessionPaused { request } => ExportEvent::SessionPaused { request: *request },
        EventKind::SessionPauseObserved { request } => {
            ExportEvent::SessionPauseObserved { request: *request }
        }
        EventKind::SessionResumed { request } => ExportEvent::SessionResumed { request: *request },
        EventKind::GoalSet {
            id: _,
            objective: _,
            owner: _,
            ceilings: _,
            derived_from: _,
        } => {
            redactions.opaque_prose += 1;
            ExportEvent::GoalSet
        }
        EventKind::GoalAllowanceInherited {
            id: _,
            continuation_budget: _,
            continuation_turns: _,
        } => ExportEvent::GoalAllowanceInherited,
        EventKind::GoalCheckConfigured {
            id: _,
            continuation_budget: _,
        } => ExportEvent::GoalCheckConfigured,
        EventKind::ContextRequested { request: _ } => {
            redactions.opaque_prose += 2;
            ExportEvent::ContextRequested
        }
        EventKind::ContextCooldown { turns: _ } => ExportEvent::ContextCooldown,
        EventKind::ContextRequestConsumed => ExportEvent::ContextRequestConsumed,
        EventKind::GoalContinuation { id: _, text: _ } => {
            redactions.opaque_prose += 1;
            ExportEvent::GoalContinuation
        }
        EventKind::GoalUpdated {
            id: _,
            objective: _,
        } => {
            redactions.opaque_prose += 1;
            ExportEvent::GoalUpdated
        }
        EventKind::GoalCheckStopped { id: _, reason: _ } => {
            redactions.opaque_prose += 1;
            ExportEvent::GoalCheckStopped
        }
        EventKind::GoalTransitioned {
            id: _,
            transition: _,
        } => {
            redactions.opaque_prose += 1;
            ExportEvent::GoalTransitioned
        }
        EventKind::PromptQueued {
            id,
            text: _,
            images,
        } => {
            redactions.opaque_prose += 1;
            ExportEvent::PromptQueued {
                id: *id,
                images: images.clone(),
            }
        }
        EventKind::PromptDeliveryExpedited { through } => {
            ExportEvent::PromptDeliveryExpedited { through: *through }
        }
        EventKind::PromptRetracted { id } => ExportEvent::PromptRetracted { id: *id },
        EventKind::UserPrompt {
            text: _,
            images,
            queued_prompt,
        } => {
            redactions.opaque_prose += 1;
            ExportEvent::UserPrompt {
                images: images.clone(),
                queued_prompt: *queued_prompt,
            }
        }
        EventKind::AssistantDelta { text: _ } => {
            redactions.opaque_prose += 1;
            ExportEvent::AssistantDelta
        }
        EventKind::AssistantMessage { text: _ } => {
            redactions.opaque_prose += 1;
            ExportEvent::AssistantMessage
        }
        EventKind::ReasoningDelta { text: _ } => {
            redactions.hidden_reasoning += 1;
            ExportEvent::ReasoningDelta
        }
        EventKind::ReasoningState { state: _ } => {
            redactions.hidden_reasoning += 1;
            ExportEvent::ReasoningState
        }
        EventKind::ToolCallStarted {
            name: _,
            args: _,
            call_id,
        } => {
            redactions.tool_material += 2 + call_id.is_some() as u64;
            ExportEvent::ToolCallStarted
        }
        EventKind::ToolCallDelta { text: _ } => {
            redactions.tool_material += 1;
            ExportEvent::ToolCallDelta
        }
        EventKind::ToolCallCompleted { ok, result: _ } => {
            redactions.tool_material += 1;
            ExportEvent::ToolCallCompleted { ok: *ok }
        }
        EventKind::ToolCallMeasured { timing } => ExportEvent::ToolCallMeasured { timing: *timing },
        EventKind::HostContextAppended { text: _ } => {
            redactions.opaque_prose += 1;
            ExportEvent::HostContextAppended
        }
        EventKind::DetachedExecStarted { id } => ExportEvent::DetachedExecStarted { id: *id },
        EventKind::DetachedExecSettled { snapshot } => project_detached_exec(snapshot, redactions),
        EventKind::QuestionRequested { request, target } => {
            redactions.opaque_prose += 1 + (request.options.len() as u64 * 2);
            ExportEvent::QuestionRequested {
                target: target.clone(),
                mode: request.mode.clone(),
                option_count: request.options.len() as u64,
                free_text: request.free_text,
            }
        }
        EventKind::QuestionSettled { outcome } => project_question_outcome(outcome, redactions),
        EventKind::PermissionRequested {
            tool: _,
            escalation,
        } => {
            redactions.permission_material += 1;
            // Exhaustive, not a `matches!` probe: a fifth escalation kind
            // that carries a path would otherwise be redacted without being
            // counted, and the ledger would under-report the one class of
            // material an operator most wants accounted for.
            match escalation {
                Escalation::NoRuleCovers { op: _, path: _ }
                | Escalation::ModeAsks {
                    mode: _,
                    op: _,
                    path: _,
                } => redactions.path += 1,
                Escalation::NoRuleCoversSubject { op: _, subject: _ }
                | Escalation::ModeAsksSubject {
                    mode: _,
                    op: _,
                    subject: _,
                } => {}
            }
            ExportEvent::PermissionRequested
        }
        EventKind::PermissionAmended { tool: _ } => {
            redactions.permission_material += 1;
            ExportEvent::PermissionAmended
        }
        EventKind::PermissionDecided {
            allowed,
            by,
            reason,
        } => {
            redactions.permission_material += 1 + by.is_some() as u64 + reason.is_some() as u64;
            ExportEvent::PermissionDecided { allowed: *allowed }
        }
        EventKind::GenerationStarted {
            attempt,
            role,
            snapshot,
        } => {
            redactions.opaque_prose += u64::from(snapshot.is_some());
            if let Some(system) = snapshot.as_ref().and_then(|value| value.system.as_ref()) {
                redactions.path += system.sources.len() as u64;
            }
            ExportEvent::GenerationStarted {
                attempt: *attempt,
                role: *role,
            }
        }
        EventKind::FallbackDecision {
            candidate,
            decision,
        } => {
            let decision = match decision {
                FallbackDecision::ReasoningDropped {
                    dropped_blocks,
                    dialects,
                } => ExportFallbackDecision::ReasoningDropped {
                    dropped_blocks: *dropped_blocks,
                    dialects: dialects.clone(),
                },
                FallbackDecision::Skipped { gaps } => {
                    ExportFallbackDecision::Skipped { gaps: gaps.clone() }
                }
                FallbackDecision::ExpressibilitySkipped { message: _ } => {
                    redactions.opaque_prose += 1;
                    ExportFallbackDecision::ExpressibilitySkipped
                }
                FallbackDecision::ResolutionSkipped { message: _ } => {
                    redactions.opaque_prose += 1;
                    ExportFallbackDecision::ResolutionSkipped
                }
            };
            ExportEvent::FallbackDecision {
                candidate: candidate.clone(),
                decision,
            }
        }
        EventKind::ExecutionObserved {
            observation,
            probe_evidence,
        } => ExportEvent::ExecutionObserved {
            observation: observation.clone(),
            probe_evidence: probe_evidence.clone(),
        },
        EventKind::ExecutionMeasured {
            observation,
            probe_evidence,
        } => ExportEvent::ExecutionMeasured {
            observation: observation.clone(),
            probe_evidence: probe_evidence.clone(),
        },
        EventKind::JudgeResponse {
            attempt,
            text,
            truncated,
        } => {
            redactions.opaque_prose += 1;
            ExportEvent::JudgeResponse {
                attempt: *attempt,
                bytes: text.len(),
                truncated: *truncated,
            }
        }
        EventKind::SandboxResidualDecided {
            path: _,
            outcome,
            recommended,
            reason: _,
            attempt,
        } => {
            redactions.path += 1;
            redactions.permission_material += 1;
            ExportEvent::SandboxResidualDecided {
                outcome: *outcome,
                recommended: *recommended,
                attempt: *attempt,
            }
        }
        EventKind::AttemptSettled { attempt, outcome } => ExportEvent::AttemptSettled {
            attempt: *attempt,
            outcome: *outcome,
        },
        EventKind::UsageUpdated {
            usage,
            cost_delta,
            attempt,
        } => ExportEvent::UsageUpdated {
            usage: *usage,
            cost_delta: cost_delta.clone(),
            attempt: *attempt,
        },
        EventKind::UsageCorrected {
            attempt,
            usage,
            cost_delta,
        } => ExportEvent::UsageCorrected {
            attempt: *attempt,
            usage: *usage,
            cost_delta: cost_delta.clone(),
        },
        EventKind::ContextWindowUpdated {
            used_tokens,
            provenance,
            window,
        } => ExportEvent::ContextWindowUpdated {
            used_tokens: *used_tokens,
            provenance: *provenance,
            window: *window,
        },
        EventKind::TurnCompleted { ok, exit_reason } => ExportEvent::TurnCompleted {
            ok: *ok,
            exit_reason: *exit_reason,
        },
        EventKind::CompactionSeeded {
            source,
            source_seq,
            prompt_revision,
            checkpoint: _,
        } => {
            redactions.opaque_prose += 1;
            ExportEvent::CompactionSeeded {
                source: *source,
                source_seq: *source_seq,
                prompt_revision: *prompt_revision,
            }
        }
        EventKind::Compacted { summary: _ } => {
            redactions.opaque_prose += 1;
            ExportEvent::Compacted
        }
        EventKind::Forked {
            source,
            at_seq,
            snapshot,
        } => {
            redactions.opaque_prose += u64::from(snapshot.is_some());
            if let Some(system) = snapshot.as_ref().and_then(|value| value.system.as_ref()) {
                redactions.path += system.sources.len() as u64;
            }
            ExportEvent::Forked {
                source: *source,
                at_seq: *at_seq,
            }
        }
        EventKind::HookCatalogueAdmitted { descriptors } => {
            redactions.path += descriptors.len() as u64;
            ExportEvent::HookCatalogueAdmitted {
                descriptors: descriptors
                    .as_slice()
                    .iter()
                    .map(|descriptor| ExportHookDescriptor {
                        id: descriptor.id(),
                        activation: descriptor.activation(),
                        source: descriptor.source(),
                        digest: descriptor.digest(),
                        ordinal: descriptor.ordinal(),
                    })
                    .collect(),
            }
        }
        EventKind::HookLifecycleReached {
            occurrence,
            subject,
            plan,
        } => ExportEvent::HookLifecycleReached {
            occurrence: occurrence.clone(),
            subject: *subject,
            plan: plan.as_slice().to_vec(),
        },
        EventKind::HookInvocationStarted {
            occurrence,
            hook,
            invocation,
            lifecycle,
            subject,
            activation,
        } => ExportEvent::HookInvocationStarted {
            occurrence: *occurrence,
            hook: *hook,
            invocation: *invocation,
            lifecycle: *lifecycle,
            subject: *subject,
            activation: *activation,
        },
        EventKind::HookInvocationSettled {
            occurrence,
            hook,
            invocation,
            result,
        } => {
            if result.feedback().is_some() {
                redactions.opaque_prose += 1;
            }
            let result = match result {
                HookResult::Outcome { outcome } => ExportHookResult::Outcome {
                    decision: outcome.decision(),
                },
                HookResult::Failure { failure } => ExportHookResult::Failure {
                    failure: failure.kind(),
                },
            };
            ExportEvent::HookInvocationSettled {
                occurrence: *occurrence,
                hook: *hook,
                invocation: *invocation,
                result,
            }
        }
        EventKind::SubagentRequested {
            invocation: _,
            task: _,
            selections: _,
        } => {
            redactions.opaque_prose += 1;
            ExportEvent::SubagentRequested
        }
        EventKind::SubagentSpawned {
            invocation: _,
            child: _,
        } => ExportEvent::SubagentSpawned,
        EventKind::SubagentSettled {
            invocation: _,
            child: _,
            terminal,
        } => {
            // Exhaustive for the same reason as `Escalation` above, and it
            // pays immediately: a `Failed` terminal now carries the
            // executor's own sentence, which is redacted here exactly as
            // `project_run_command` redacts it on the receipt.
            match terminal {
                SubagentTerminal::Refused { failure: _ } => redactions.opaque_prose += 1,
                SubagentTerminal::Failed { settlement } => {
                    redactions.raw_error += u64::from(settlement.diagnostic().is_some());
                }
                SubagentTerminal::Returned { final_message: _ }
                | SubagentTerminal::ReturnBlocked {
                    final_message: _,
                    hook_invocation: _,
                } => {}
            }
            ExportEvent::SubagentSettled
        }
        EventKind::SkillCatalogueAdmitted {
            generation: _,
            descriptors: _,
        } => ExportEvent::SkillCatalogueAdmitted,
        EventKind::SkillInstructionAdmitted {
            invocation: _,
            skill: _,
            catalogue_generation: _,
            source: _,
            location: _,
            source_digest: _,
            admission: _,
            rendered: _,
            model: _,
            effort: _,
        } => {
            redactions.opaque_prose += 1;
            ExportEvent::SkillInstructionAdmitted
        }
        EventKind::SkillResourceAdmitted {
            invocation: _,
            skill: _,
            location: _,
            content: _,
        } => {
            redactions.opaque_prose += 1;
            ExportEvent::SkillResourceAdmitted
        }
        EventKind::Error { message: _, kind } => {
            redactions.raw_error += 1;
            ExportEvent::Error { error_kind: *kind }
        }
    }
}

#[derive(Default)]
struct EvidenceValidation {
    keys: HashSet<EvidenceKey>,
    contexts: HashMap<RunInvocationId, (RunId, Seq)>,
    generations: HashMap<AttemptRef, (RunInvocationId, GenerationRole, Seq)>,
    starts: HashSet<AttemptRef>,
    admissions: HashMap<RunInvocationId, (RunId, Seq)>,
}

impl EvidenceValidation {
    fn record(
        &mut self,
        session: SessionId,
        position: Seq,
        record: &EvidenceRecord,
    ) -> Result<(), ExportError> {
        if record.version() != EVIDENCE_SCHEMA_VERSION {
            return evidence_error(
                session,
                position,
                ExportEvidenceFailure::UnsupportedVersion {
                    found: record.version(),
                    required: EVIDENCE_SCHEMA_VERSION,
                },
            );
        }
        let key = record.key();
        if !self.keys.insert(key.clone()) {
            return evidence_error(session, position, ExportEvidenceFailure::Duplicate { key });
        }
        match record {
            EvidenceRecord::ToolCatalogue(_) => {}
            EvidenceRecord::RunContext(context) => {
                if context.session() != session {
                    return evidence_error(
                        session,
                        position,
                        ExportEvidenceFailure::SessionMismatch {
                            recorded: context.session(),
                        },
                    );
                }
                if context.run_id().session_id() != session {
                    return evidence_error(
                        session,
                        position,
                        ExportEvidenceFailure::RunMismatch {
                            invocation: context.invocation(),
                        },
                    );
                }
                if let Some((admitted, _)) = self.admissions.get(&context.invocation())
                    && *admitted != context.run_id()
                {
                    return evidence_error(
                        session,
                        position,
                        ExportEvidenceFailure::RunMismatch {
                            invocation: context.invocation(),
                        },
                    );
                }
                self.contexts
                    .insert(context.invocation(), (context.run_id(), position));
            }
            EvidenceRecord::GenerationAdmission(admission) => {
                if admission.session() != session {
                    return evidence_error(
                        session,
                        position,
                        ExportEvidenceFailure::SessionMismatch {
                            recorded: admission.session(),
                        },
                    );
                }
                if !self.contexts.contains_key(&admission.invocation()) {
                    return evidence_error(
                        session,
                        position,
                        ExportEvidenceFailure::MissingRunContext {
                            invocation: admission.invocation(),
                        },
                    );
                }
                if let Some((existing_invocation, _, _)) =
                    self.generations.get(&admission.attempt())
                {
                    return evidence_error(
                        session,
                        position,
                        ExportEvidenceFailure::AttemptOwnershipConflict {
                            attempt: admission.attempt(),
                            existing_invocation: *existing_invocation,
                            requested_invocation: admission.invocation(),
                        },
                    );
                }
                self.generations.insert(
                    admission.attempt(),
                    (admission.invocation(), admission.role(), position),
                );
            }
        }
        Ok(())
    }

    fn transaction(
        &mut self,
        session: SessionId,
        position: Seq,
        transaction: &crate::run::RunTransaction,
    ) -> Result<(), ExportError> {
        if transaction.run_id().session_id() != session {
            return evidence_error(
                session,
                position,
                ExportEvidenceFailure::RunMismatch {
                    invocation: RunInvocationId::from_uuid(transaction.run_id().nonce()),
                },
            );
        }
        if let RunCommand::CommandAdmitted { request_id, .. } = transaction.command() {
            let invocation = RunInvocationId::from_uuid(request_id.uuid());
            if let Some((context, _)) = self.contexts.get(&invocation) {
                if *context != transaction.run_id() {
                    return evidence_error(
                        session,
                        position,
                        ExportEvidenceFailure::RunMismatch { invocation },
                    );
                }
                return evidence_error(
                    session,
                    position,
                    ExportEvidenceFailure::OutOfOrder {
                        key: EvidenceKey::RunContext {
                            session,
                            invocation,
                        },
                    },
                );
            }
            self.admissions
                .insert(invocation, (transaction.run_id(), position));
        }
        Ok(())
    }

    fn event(
        &mut self,
        session: SessionId,
        position: Seq,
        event: &EventKind,
    ) -> Result<(), ExportError> {
        let EventKind::GenerationStarted { attempt, role, .. } = event else {
            return Ok(());
        };
        let Some((_, admitted_role, _)) = self.generations.get(attempt) else {
            return evidence_error(
                session,
                position,
                ExportEvidenceFailure::MissingGenerationAdmission { attempt: *attempt },
            );
        };
        if admitted_role != role {
            return evidence_error(
                session,
                position,
                ExportEvidenceFailure::GenerationRoleMismatch { attempt: *attempt },
            );
        }
        if !self.starts.insert(*attempt) {
            let invocation = self
                .generations
                .get(attempt)
                .map(|(invocation, _, _)| *invocation)
                .expect("generation admission was checked above");
            return evidence_error(
                session,
                position,
                ExportEvidenceFailure::Duplicate {
                    key: EvidenceKey::GenerationAdmission {
                        session,
                        invocation,
                        attempt: *attempt,
                    },
                },
            );
        }
        Ok(())
    }

    fn finish(self, session: SessionId, last: Seq) -> Result<(), ExportError> {
        for (invocation, (run_id, position)) in self.admissions {
            if self.contexts.get(&invocation).map(|(found, _)| *found) != Some(run_id) {
                return evidence_error(
                    session,
                    position,
                    ExportEvidenceFailure::MissingRunContext { invocation },
                );
            }
        }
        if self
            .generations
            .keys()
            .any(|attempt| !self.starts.contains(attempt))
        {
            return evidence_error(session, last, ExportEvidenceFailure::MissingRunEvidence);
        }
        Ok(())
    }
}

fn evidence_error<T>(
    session: SessionId,
    position: Seq,
    failure: ExportEvidenceFailure,
) -> Result<T, ExportError> {
    Err(ExportError::Evidence {
        session,
        position,
        failure,
    })
}

#[derive(Default)]
struct Redactions {
    opaque_prose: u64,
    hidden_reasoning: u64,
    path: u64,
    tool_material: u64,
    permission_material: u64,
    raw_error: u64,
    blob_payload: u64,
}

impl Redactions {
    fn rows(self) -> Vec<RedactionCount> {
        [
            (RedactionCategory::OpaqueProse, self.opaque_prose),
            (RedactionCategory::HiddenReasoning, self.hidden_reasoning),
            (RedactionCategory::Path, self.path),
            (RedactionCategory::ToolMaterial, self.tool_material),
            (
                RedactionCategory::PermissionMaterial,
                self.permission_material,
            ),
            (RedactionCategory::RawError, self.raw_error),
            (RedactionCategory::BlobPayload, self.blob_payload),
        ]
        .into_iter()
        .filter_map(|(category, count)| (count != 0).then_some(RedactionCount { category, count }))
        .collect()
    }
}

fn project_detached_exec(
    snapshot: &crate::permit::DetachedExecSnapshot,
    redactions: &mut Redactions,
) -> ExportEvent {
    use crate::permit::DetachedExecOutcome;
    let crate::permit::DetachedExecSnapshot {
        id,
        outcome,
        stdout_tail,
        stderr_tail,
        stdout_omitted,
        stderr_omitted,
    } = snapshot;
    let (state, exit_code) = match outcome {
        Some(DetachedExecOutcome::Exited { code, result }) => {
            redactions.tool_material += 1;
            let _ = result;
            ("exited", Some(*code))
        }
        Some(DetachedExecOutcome::Signalled { signal, result }) => {
            redactions.tool_material += 1;
            let _ = (signal, result);
            ("signalled", None)
        }
        Some(DetachedExecOutcome::Cancelled) => ("cancelled", None),
        Some(DetachedExecOutcome::TimedOut { timeout_ms }) => {
            let _ = timeout_ms;
            ("timed_out", None)
        }
        Some(DetachedExecOutcome::Interrupted) => ("interrupted", None),
        Some(DetachedExecOutcome::Failed { reason }) => {
            redactions.raw_error += 1;
            let _ = reason;
            ("failed", None)
        }
        None => ("running", None),
    };
    redactions.tool_material +=
        u64::from(!stdout_tail.is_empty()) + u64::from(!stderr_tail.is_empty());
    ExportEvent::DetachedExecSettled {
        id: *id,
        state: state.to_owned(),
        exit_code,
        stdout_omitted: *stdout_omitted,
        stderr_omitted: *stderr_omitted,
    }
}

fn project_question_outcome(outcome: &QuestionOutcome, redactions: &mut Redactions) -> ExportEvent {
    let outcome = match outcome {
        QuestionOutcome::Answered(answer) => {
            redactions.opaque_prose += match answer {
                crate::question::QuestionAnswer::Multiple(values) => values.len() as u64,
                crate::question::QuestionAnswer::Single(_)
                | crate::question::QuestionAnswer::FreeText(_) => 1,
                crate::question::QuestionAnswer::Form(value) => value.len() as u64,
            };
            ExportQuestionOutcome::Answered
        }
        QuestionOutcome::Declined => ExportQuestionOutcome::Declined,
        QuestionOutcome::Cancelled => ExportQuestionOutcome::Cancelled,
        QuestionOutcome::Unavailable => ExportQuestionOutcome::Unavailable,
    };
    ExportEvent::QuestionSettled { outcome }
}
