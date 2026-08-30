//! RED contract for MCP lifecycle ownership in M4 S7.
//!
//! The operation factory and its durable continuation recorder are the only
//! producers of the two MRTR lifecycle points. The tests intentionally attach
//! the final dispatcher at that production boundary.

use std::sync::{Arc, Mutex};

use ego_core::config::{ConfigLayer, Source, Trust};
use ego_core::event::hook::{
    HookActivation, HookContentDigest, HookDecision, HookFailure, HookInvocationId, HookLifecycle,
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
        Ok(())
    }
    fn load(&self, _id: &str) -> Result<ContinuationRecord, ContinuationError> {
        Err(ContinuationError::NotFound)
    }
    fn claim(&self, id: &str, now: Timestamp) -> Result<ClaimedContinuation, ContinuationError> {
        let mut pending = self.pending.lock().expect("continuation record");
        let record = pending.take().ok_or(ContinuationError::NotFound)?;
        if record.id() != id {
            *pending = Some(record);
            return Err(ContinuationError::NotFound);
        }
        let claimed = record.claim(now)?;
        self.events
            .lock()
            .expect("continuation events")
            .push("claim");
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
    let journal = Arc::new(Continuations::default());
    let journal_port: Arc<dyn ContinuationJournal> = journal.clone();
    let recorder = Arc::new(DurableMcpInputRequiredRecorder::new(journal_port));
    let port = RecordingPort::default();
    let dispatcher = dispatcher(port.clone());
    let factory =
        McpStdioOperationFactory::new(recorder, 64 * 1024, std::time::Duration::from_secs(30))
            .expect("MCP operation owner")
            .with_hook_dispatcher(dispatcher);
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
    futures::executor::block_on(factory.dispatch_lifecycle(HookLifecycle::Elicitation))
        .expect("request lifecycle after durable record");
    let continuation_id = journal
        .recorded_id()
        .expect("operation owner durably records continuation before Elicitation");
    journal
        .accept_answer(&continuation_id, br#"{"choice":"yes"}"#, Timestamp::EPOCH)
        .expect("answer is accepted durably before ElicitationResult");
    let retry = state
        .resume(answer)
        .expect("accepted answer resumes original operation");
    futures::executor::block_on(factory.dispatch_lifecycle(HookLifecycle::ElicitationResult))
        .expect("result lifecycle before durable claim");
    journal
        .claim(&continuation_id, Timestamp::EPOCH)
        .expect("continuation is claimed only after ElicitationResult");
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
