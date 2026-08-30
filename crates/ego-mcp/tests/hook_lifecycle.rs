//! RED contract for MCP lifecycle ownership in M4 S7.
//!
//! The operation factory and its durable continuation recorder are the only
//! producers of the two MRTR lifecycle points. The tests intentionally attach
//! the final dispatcher at that production boundary.

use std::sync::{Arc, Mutex};

use ego_core::config::{ConfigLayer, Source, Trust};
use ego_core::event::hook::{
    HookActivation, HookContentDigest, HookDecision, HookInvocationId, HookLifecycle,
    HookOccurrenceKey, HookOutcome, HookPlan, HookResult,
};
use ego_core::event::id::Producer;
use ego_core::event::time::Timestamp;
use ego_core::hook::{
    HookCatalogue, HookEvent, HookFuture, HookLifecycleDispatcher, HookOccurrence, HookPort,
    HookReachFuture, HookReachedEvidence, HookRecordFuture, HookRunner, HookSpec, HookStartFuture,
    HookStartedEvidence,
};
use ego_core::suspension::{
    ClaimedContinuation, ContinuationError, ContinuationJournal, ContinuationRecord,
    ContinuationState,
};
use ego_mcp::run::McpContinuationOwner;
use ego_mcp::{
    DurableMcpInputRequiredRecorder, McpInputMapping, McpInputRequired, McpInputResponse,
    McpInputState, McpStdioOperationFactory,
};

const MCP_OWNERS: [(&str, HookLifecycle); 2] = [
    (
        "McpOperationFactory::project_outcome durable request",
        HookLifecycle::Elicitation,
    ),
    (
        "MCP journal accepted answer before guarded claim",
        HookLifecycle::ElicitationResult,
    ),
];

fn hook_catalogue() -> HookCatalogue {
    let source = "[[hooks]]\nevent = \"Elicitation\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"continue\"\n[[hooks]]\nevent = \"ElicitationResult\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"continue\"\n";
    HookCatalogue::admit_typed(
        &[ConfigLayer::parse(Source::User, source).expect("admitted MCP hooks")],
        &[],
        Trust::Trusted,
    )
    .expect("MCP hook catalogue")
}

#[derive(Clone, Default)]
struct Trace(Arc<Mutex<Vec<String>>>);

impl Trace {
    fn push(&self, event: impl Into<String>) {
        self.0
            .lock()
            .expect("MCP lifecycle trace")
            .push(event.into());
    }

    fn events(&self) -> Vec<String> {
        self.0.lock().expect("MCP lifecycle trace").clone()
    }
}

#[derive(Clone, Default)]
struct RecordingPort {
    calls: Arc<Mutex<Vec<HookLifecycle>>>,
    trace: Trace,
    mode: HookMode,
}

#[derive(Clone, Copy, Default)]
enum HookMode {
    #[default]
    Continue,
    BlockResult,
    UncertainResult,
}

impl RecordingPort {
    fn calls(&self) -> Vec<HookLifecycle> {
        self.calls.lock().expect("MCP hook calls").clone()
    }

    fn with_trace(trace: Trace) -> Self {
        Self {
            calls: Arc::default(),
            trace,
            mode: HookMode::Continue,
        }
    }

    fn blocking_with_trace(trace: Trace) -> Self {
        Self {
            calls: Arc::default(),
            trace,
            mode: HookMode::BlockResult,
        }
    }

    fn uncertain_with_trace(trace: Trace) -> Self {
        Self {
            calls: Arc::default(),
            trace,
            mode: HookMode::UncertainResult,
        }
    }
}

impl HookPort for RecordingPort {
    fn reached<'a>(
        &'a self,
        producer: &'a ego_core::event::id::Producer,
        occurrence: &'a HookOccurrence,
        plan: &'a HookPlan,
    ) -> HookReachFuture<'a> {
        let evidence = HookReachedEvidence::new(
            ego_core::event::id::EventId::new(),
            occurrence.key().clone(),
            occurrence.event().subject(),
            producer.clone(),
            plan.clone(),
        );
        Box::pin(std::future::ready(Ok(
            ego_core::hook::HookReachOutcome::Fresh(evidence),
        )))
    }
    fn started<'a>(
        &'a self,
        spec: &'a HookSpec,
        occurrence: &'a HookOccurrence,
        reached: &'a HookReachedEvidence,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> HookStartFuture<'a> {
        self.calls
            .lock()
            .expect("MCP hook calls")
            .push(occurrence.event().lifecycle());
        self.trace.push(occurrence.event().lifecycle().as_str());
        Box::pin(std::future::ready(Ok(HookStartedEvidence::new(
            reached.event(),
            reached.event(),
            spec.id(),
            invocation,
            activation,
        ))))
    }

    fn invoke<'a>(
        &'a self,
        _spec: &'a HookSpec,
        event: ego_core::hook::HookEvent,
    ) -> HookFuture<'a> {
        let outcome = match (self.mode, event.lifecycle()) {
            (HookMode::Continue, _) => HookOutcome::new(HookDecision::Continue, None),
            (HookMode::BlockResult, HookLifecycle::ElicitationResult) => {
                HookOutcome::new(HookDecision::Block, None)
            }
            (HookMode::BlockResult | HookMode::UncertainResult, _) => {
                HookOutcome::new(HookDecision::Continue, None)
            }
        };
        Box::pin(std::future::ready(Ok(outcome)))
    }

    fn settled<'a>(
        &'a self,
        spec: &'a HookSpec,
        _started: HookStartedEvidence,
        _result: &'a HookResult,
    ) -> HookRecordFuture<'a> {
        if matches!(self.mode, HookMode::UncertainResult)
            && spec.event_name() == HookLifecycle::ElicitationResult.as_str()
        {
            return Box::pin(std::future::ready(Err(ego_core::hook::HookRunError::new(
                "simulated post-answer delivery uncertainty",
            ))));
        }
        Box::pin(std::future::ready(Ok(())))
    }
}

fn dispatcher(port: RecordingPort) -> HookLifecycleDispatcher {
    HookLifecycleDispatcher::new(
        HookRunner::new_with_durable_started(port, Vec::<HookStartedEvidence>::new()),
        hook_catalogue(),
    )
}

fn occurrence(lifecycle: HookLifecycle) -> HookOccurrence {
    HookOccurrence::new(
        HookOccurrenceKey::continuation(lifecycle, format!("mcp-{}", "0".repeat(64)))
            .expect("matching continuation occurrence owner"),
        HookEvent::session(lifecycle),
    )
    .expect("matching lifecycle occurrence")
}

fn required() -> McpInputRequired {
    McpInputRequired::elicitation(
        br#"{"type":"object","properties":{"choice":{"type":"string"}},"required":["choice"],"additionalProperties":false}"#,
        "answer",
        b"opaque-request-state",
    )
    .expect("current MRTR elicitation form")
}

#[derive(Default)]
struct Continuations {
    events: Mutex<Vec<&'static str>>,
    pending: Mutex<Option<ContinuationRecord>>,
    trace: Trace,
}
impl Continuations {
    fn records(&self) -> usize {
        self.events
            .lock()
            .expect("continuation events")
            .iter()
            .filter(|event| **event == "record")
            .count()
    }

    fn recorded_id(&self) -> Option<String> {
        self.pending
            .lock()
            .expect("continuation record")
            .as_ref()
            .map(|record| record.id().to_owned())
    }

    fn events(&self) -> Vec<&'static str> {
        self.events.lock().expect("continuation events").clone()
    }

    fn with_trace(trace: Trace) -> Self {
        Self {
            events: Mutex::default(),
            pending: Mutex::default(),
            trace,
        }
    }

    fn state(&self) -> Option<ContinuationState> {
        self.pending
            .lock()
            .expect("continuation record")
            .as_ref()
            .map(ContinuationRecord::state)
    }
}
impl ContinuationJournal for Continuations {
    fn record(&self, record: ContinuationRecord) -> Result<(), ContinuationError> {
        let mut pending = self.pending.lock().expect("continuation record");
        if pending.is_some() {
            return Err(ContinuationError::InvalidTransition);
        }
        *pending = Some(record);
        self.events
            .lock()
            .expect("continuation events")
            .push("record");
        self.trace.push("record");
        Ok(())
    }
    fn accept_answer(
        &self,
        id: &str,
        answer: &[u8],
        now: Timestamp,
    ) -> Result<(), ContinuationError> {
        let mut pending = self.pending.lock().expect("continuation record");
        let record = pending.as_mut().ok_or(ContinuationError::NotFound)?;
        if record.id() != id {
            return Err(ContinuationError::NotFound);
        }
        record.accept_answer(answer.to_vec(), now)?;
        self.events
            .lock()
            .expect("continuation events")
            .push("accepted_answer");
        self.trace.push("accepted_answer");
        Ok(())
    }
    fn load(&self, id: &str) -> Result<ContinuationRecord, ContinuationError> {
        let pending = self.pending.lock().expect("continuation record");
        let record = pending
            .as_ref()
            .filter(|record| record.id() == id)
            .ok_or(ContinuationError::NotFound)?;
        serde_json::from_slice(
            &serde_json::to_vec(record).map_err(|_| ContinuationError::Corrupt("encode".into()))?,
        )
        .map_err(|_| ContinuationError::Corrupt("decode".into()))
    }
    fn claim(&self, id: &str, now: Timestamp) -> Result<ClaimedContinuation, ContinuationError> {
        let mut pending = self.pending.lock().expect("continuation record");
        let record = pending.take().ok_or(ContinuationError::NotFound)?;
        if record.id() != id {
            *pending = Some(record);
            return Err(ContinuationError::NotFound);
        }
        let encoded =
            serde_json::to_vec(&record).map_err(|_| ContinuationError::Corrupt("encode".into()))?;
        let claimed = match record.claim(now) {
            Ok(claimed) => claimed,
            Err(error) => {
                *pending = Some(
                    serde_json::from_slice(&encoded)
                        .map_err(|_| ContinuationError::Corrupt("decode".into()))?,
                );
                return Err(error);
            }
        };
        self.events
            .lock()
            .expect("continuation events")
            .push("claim");
        self.trace.push("claim");
        Ok(claimed)
    }
    fn settle(
        &self,
        _claimed: ClaimedContinuation,
        _state: ContinuationState,
    ) -> Result<(), ContinuationError> {
        self.events
            .lock()
            .expect("continuation events")
            .push("settle");
        Ok(())
    }
}

#[test]
fn mcp_matrix_has_one_owner_for_each_mrtr_lifecycle() {
    assert_eq!(
        MCP_OWNERS,
        [
            (
                "McpOperationFactory::project_outcome durable request",
                HookLifecycle::Elicitation
            ),
            (
                "MCP journal accepted answer before guarded claim",
                HookLifecycle::ElicitationResult
            ),
        ]
    );
}

#[test]
fn operation_factory_projects_elicitation_after_record_then_result_before_claim() {
    let trace = Trace::default();
    let journal = Arc::new(Continuations::with_trace(trace.clone()));
    let journal_port: Arc<dyn ContinuationJournal> = journal.clone();
    let recorder = Arc::new(DurableMcpInputRequiredRecorder::new(journal_port));
    let port = RecordingPort::with_trace(trace.clone());
    let owner = McpContinuationOwner::new(
        McpStdioOperationFactory::new(recorder, 64 * 1024, std::time::Duration::from_secs(30))
            .expect("MCP operation owner"),
        journal.clone(),
        dispatcher(port.clone()),
    );
    let state = McpInputState::from_mapping(
        "server".parse().expect("server"),
        "tools/call",
        br#"{"name":"lookup","arguments":{}}"#,
        McpInputMapping::map(required()).expect("one accepted MRTR form"),
    )
    .expect("durable input-required state");
    let request_id = state.request_id().clone();
    let foreign = futures::executor::block_on(owner.execute_input_required(state, required()))
        .expect("actual operation owner projects input-required outcome");
    let foreign = match foreign {
        ego_core::mcp::ToolExecution::InputRequired(foreign) => foreign,
        ego_core::mcp::ToolExecution::Complete(_) => {
            panic!("MRTR input-required outcome must remain foreign input-required")
        }
    };
    let continuation_id = journal
        .recorded_id()
        .expect("operation owner durably records continuation before Elicitation");
    assert_eq!(foreign.continuation_id(), continuation_id);
    let retry = futures::executor::block_on(owner.accept_answer(
        &continuation_id,
        McpInputResponse::new(&request_id, br#"{"choice":"yes"}"#).expect("validated answer"),
        Timestamp::EPOCH,
    ))
    .expect("answer is accepted, hooked, claimed, and retried by the run owner");
    assert_eq!(retry.method(), "tools/call");
    assert!(
        std::str::from_utf8(retry.body())
            .expect("retry body")
            .contains("inputResponses")
    );
    assert_eq!(
        journal.records(),
        1,
        "input-required result crosses the durable continuation boundary once"
    );
    assert_eq!(
        journal.events(),
        vec!["record", "accepted_answer", "claim"],
        "record, accepted answer, hook points, and claim preserve owner order"
    );
    assert_eq!(
        trace.events(),
        vec![
            "record",
            "Elicitation",
            "accepted_answer",
            "ElicitationResult",
            "claim"
        ],
        "durable record precedes Elicitation and accepted answer precedes result hook/claim"
    );
    assert_eq!(
        port.calls(),
        vec![HookLifecycle::Elicitation, HookLifecycle::ElicitationResult]
    );
}

#[test]
fn mcp_pre_block_and_uncertainty_leave_accepted_answer_unclaimed_without_reentry() {
    let trace = Trace::default();
    let journal = Arc::new(Continuations::with_trace(trace.clone()));
    let journal_port: Arc<dyn ContinuationJournal> = journal.clone();
    let recorder = Arc::new(DurableMcpInputRequiredRecorder::new(journal_port));
    let port = RecordingPort::blocking_with_trace(trace.clone());
    let owner = McpContinuationOwner::new(
        McpStdioOperationFactory::new(recorder, 64 * 1024, std::time::Duration::from_secs(30))
            .expect("MCP operation owner"),
        journal.clone(),
        dispatcher(port.clone()),
    );
    let state = McpInputState::required(
        "server".parse().expect("server"),
        "tools/call",
        "call-block",
        br#"{"type":"object","properties":{"choice":{"type":"string"}},"required":["choice"],"additionalProperties":false}"#,
        br#"{"name":"lookup","arguments":{}}"#,
    )
    .expect("MRTR state");
    let request_id = state.request_id().clone();
    let _ = futures::executor::block_on(owner.execute_input_required(state, required()))
        .expect("input-required projection");
    let id = journal.recorded_id().expect("recorded continuation");
    let result = futures::executor::block_on(owner.accept_answer(
        &id,
        McpInputResponse::new(&request_id, br#"{"choice":"yes"}"#).expect("schema-valid answer"),
        Timestamp::EPOCH,
    ));
    assert!(
        result.is_err(),
        "a pre-effect block/uncertainty must fail closed"
    );
    assert_eq!(journal.state(), Some(ContinuationState::Accepted));
    assert!(!journal.events().iter().any(|event| *event == "claim"));
    assert_eq!(port.calls(), vec![HookLifecycle::Elicitation]);

    let trace = Trace::default();
    let journal = Arc::new(Continuations::with_trace(trace.clone()));
    let journal_port: Arc<dyn ContinuationJournal> = journal.clone();
    let recorder = Arc::new(DurableMcpInputRequiredRecorder::new(journal_port));
    let port = RecordingPort::uncertain_with_trace(trace.clone());
    let owner = McpContinuationOwner::new(
        McpStdioOperationFactory::new(recorder, 64 * 1024, std::time::Duration::from_secs(30))
            .expect("MCP operation owner"),
        journal.clone(),
        dispatcher(port.clone()),
    );
    let state = McpInputState::required(
        "server".parse().expect("server"),
        "tools/call",
        "call-uncertain",
        br#"{"type":"object","properties":{"choice":{"type":"string"}},"required":["choice"],"additionalProperties":false}"#,
        br#"{"name":"lookup","arguments":{}}"#,
    )
    .expect("MRTR state");
    let request_id = state.request_id().clone();
    let _ = futures::executor::block_on(owner.execute_input_required(state, required()))
        .expect("input-required projection");
    let id = journal.recorded_id().expect("recorded continuation");
    let result = futures::executor::block_on(owner.accept_answer(
        &id,
        McpInputResponse::new(&request_id, br#"{"choice":"yes"}"#).expect("schema-valid answer"),
        Timestamp::EPOCH,
    ));
    assert!(result.is_err(), "uncertain result delivery must not claim");
    assert_eq!(journal.state(), Some(ContinuationState::Accepted));
    assert!(!journal.events().iter().any(|event| *event == "claim"));
    assert_eq!(
        port.calls(),
        vec![HookLifecycle::Elicitation, HookLifecycle::ElicitationResult]
    );
}

#[test]
fn accepted_answer_is_single_use_and_delivery_uncertainty_is_not_retried() {
    let state = McpInputState::required(
        "server".parse().expect("server"),
        "tools/call",
        "call-1",
        br#"{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}"#,
        br#"{"name":"confirm","arguments":{}}"#,
    ).expect("MRTR state");
    let request_id = state.request_id().clone();
    let response = McpInputResponse::new(&request_id, br#"{"ok":true}"#).expect("answer");
    let retry = state.resume(response).expect("first answer claim");
    assert!(McpInputResponse::new(&request_id, br#"{"ok":true}"#).is_err());
    assert_eq!(retry.call_id(), "call-1");
}

#[test]
fn mcp_hook_effect_producer_cannot_reenter_mrtr() {
    let location =
        ego_core::event::hook::HookSourceLocation::parse("hooks/mcp.toml").expect("hook location");
    let digest: HookContentDigest =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .parse()
            .expect("hook digest");
    let hook = ego_core::event::hook::HookId::derive(
        ego_core::event::hook::HookSourceAuthority::Project,
        &location,
        &digest,
        0,
    );
    let port = RecordingPort::default();
    let dispatcher = dispatcher(port.clone());
    let _ = futures::executor::block_on(
        dispatcher.dispatch(Producer::Hook(hook), occurrence(HookLifecycle::Elicitation)),
    );
    assert!(!Producer::Hook(hook).may_trigger_hooks());
    assert!(
        port.calls().is_empty(),
        "hook-originated MRTR effects cannot recursively emit lifecycle events"
    );
}
