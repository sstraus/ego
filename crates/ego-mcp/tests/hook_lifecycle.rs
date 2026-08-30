//! RED contract for MCP lifecycle ownership in M4 S7.
//!
//! The operation factory and its durable continuation recorder are the only
//! producers of the two MRTR lifecycle points. The tests intentionally attach
//! the final dispatcher at that production boundary.

use std::sync::{Arc, Mutex};

use ego_core::config::{ConfigLayer, Source, Trust};
use ego_core::event::hook::{
    HookActivation, HookContentDigest, HookDecision, HookFailure, HookInvocationId, HookLifecycle,
    HookOccurrenceKey, HookOccurrenceOwner, HookOutcome, HookPlan, HookResult,
};
use ego_core::event::id::{Producer, SessionId};
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
        "McpInputState resume before guarded claim",
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
struct RecordingPort {
    calls: Arc<Mutex<Vec<HookLifecycle>>>,
}

impl RecordingPort {
    fn calls(&self) -> Vec<HookLifecycle> {
        self.calls.lock().expect("MCP hook calls").clone()
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
        _event: ego_core::hook::HookEvent,
    ) -> HookFuture<'a> {
        Box::pin(std::future::ready(Ok(HookOutcome::new(
            HookDecision::Continue,
            None,
        ))))
    }

    fn settled<'a>(
        &'a self,
        _spec: &'a HookSpec,
        _started: HookStartedEvidence,
        _result: &'a HookResult,
    ) -> HookRecordFuture<'a> {
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
        HookOccurrenceKey::new(
            lifecycle,
            HookOccurrenceOwner::Session {
                session: SessionId::new(),
            },
        ),
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
    records: Mutex<usize>,
}
impl Continuations {
    fn records(&self) -> usize {
        *self.records.lock().expect("continuation records")
    }
}
impl ContinuationJournal for Continuations {
    fn record(&self, _record: ContinuationRecord) -> Result<(), ContinuationError> {
        *self.records.lock().expect("continuation records") += 1;
        Ok(())
    }
    fn accept_answer(
        &self,
        _id: &str,
        _answer: &[u8],
        _now: Timestamp,
    ) -> Result<(), ContinuationError> {
        Err(ContinuationError::Unavailable(
            "answer belongs to the operation owner".to_owned(),
        ))
    }
    fn load(&self, _id: &str) -> Result<ContinuationRecord, ContinuationError> {
        Err(ContinuationError::NotFound)
    }
    fn claim(&self, _id: &str, _now: Timestamp) -> Result<ClaimedContinuation, ContinuationError> {
        Err(ContinuationError::NotFound)
    }
    fn settle(
        &self,
        _claimed: ClaimedContinuation,
        _state: ContinuationState,
    ) -> Result<(), ContinuationError> {
        Err(ContinuationError::Unavailable(
            "settlement belongs to the operation owner".to_owned(),
        ))
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
                "McpInputState resume before guarded claim",
                HookLifecycle::ElicitationResult
            ),
        ]
    );
}

#[test]
fn operation_factory_projects_elicitation_after_record_then_result_before_claim() {
    let journal = Arc::new(Continuations::default());
    let journal_port: Arc<dyn ContinuationJournal> = journal.clone();
    let recorder = Arc::new(DurableMcpInputRequiredRecorder::new(journal_port));
    let port = RecordingPort::default();
    let dispatcher = dispatcher(port.clone());
    let factory =
        McpStdioOperationFactory::new(recorder, 64 * 1024, std::time::Duration::from_secs(30))
            .expect("MCP operation owner")
            .with_hook_dispatcher(dispatcher);
    futures::executor::block_on(factory.dispatch_lifecycle(HookLifecycle::Elicitation))
        .expect("request lifecycle before durable record");
    let mapped = McpInputMapping::map(required()).expect("one accepted MRTR form");
    let state = McpInputState::from_mapping(
        "server".parse().expect("server"),
        "tools/call",
        br#"{"name":"lookup","arguments":{}}"#,
        mapped,
    )
    .expect("durable input-required state");
    let answer = McpInputResponse::new(state.request_id(), br#"{"choice":"yes"}"#)
        .expect("validated answer");
    let retry = state
        .resume(answer)
        .expect("accepted answer resumes original operation");
    futures::executor::block_on(factory.dispatch_lifecycle(HookLifecycle::ElicitationResult))
        .expect("result lifecycle before durable claim");
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
