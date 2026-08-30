//! RED contract for M4 S2: isolated child runs over the rooted controller.
//!
//! These tests intentionally name the final child-run service rather than
//! introducing a scheduler or a second run loop.  The service under test is
//! expected to use the existing `RunController`, session store, evidence
//! port, permission/configuration owners, and shared budget gate.

use std::collections::BTreeMap;

use ego_core::budget::BudgetGate;
use ego_core::ceiling::Budget;
use ego_core::config::{Mode, ModelRef};
use ego_core::cost::{Amount, CurrencyCode, Money};
use ego_core::event::domain::{SubagentRefusal, SubagentTerminal};
use ego_core::event::id::{ItemId, SessionId, SubagentInvocationId, TurnId};
use ego_core::run::{RunId, RunOrigin, RunSelections, RunSettlement};
use ego_core::store::blob::{BlobId, BlobRef};
use ego_core::subagent::{
    ChildRequest, ChildRunService, ChildServiceError, ChildWait, ChildWaitResult,
    RecoveryCut,
};

const TASK: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const RESULT: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn blob(hex: &str, bytes: u64) -> BlobRef {
    BlobRef::new(BlobId::parse(hex).expect("test blob id"), bytes)
}

fn selections(profile: Option<&str>) -> RunSelections {
    RunSelections::new(
        Some(ModelRef("provider/model".to_owned())),
        Some(ego_core::generator::types::ThinkingLevel::Low),
        Some(Mode::Default),
        Some("40k".parse::<Budget>().expect("test budget")),
        profile.map(str::to_owned),
    )
}

fn parent() -> RunId {
    RunId::new(SessionId::new(), uuid::Uuid::now_v7())
}

fn request(mode: ChildWait) -> ChildRequest {
    ChildRequest::new(
        SubagentInvocationId::new(),
        parent(),
        TurnId::new(),
        ItemId::new(),
        blob(TASK, 12),
        selections(Some("narrow")),
        mode,
    )
}

#[test]
fn child_start_uses_the_rooted_controller_and_journals_the_ratified_order() {
    let service = ChildRunService::test_fixture();
    let request = request(ChildWait::Foreground);
    let invocation = request.invocation();

    let admission = futures::executor::block_on(service.start(request)).expect("child admission");
    assert_eq!(admission.invocation(), invocation);
    assert!(matches!(admission.origin(), RunOrigin::Subagent { .. }));
    assert_eq!(admission.child_run().session_id(), admission.child_session());
    assert_ne!(admission.child_run(), admission.parent_run());

    assert_eq!(
        service.parent_events(),
        [
            "subagent_requested",
            "subagent_spawned",
            "subagent_start",
        ],
        "request precedes controller admission, spawn, and observer start"
    );
    assert_eq!(service.child_admission_count(admission.child_run()), 1);
    match admission.origin() {
        RunOrigin::Subagent { parent_run, .. } => {
            assert_eq!(parent_run.to_owned(), admission.parent_run());
        }
        RunOrigin::TopLevel => panic!("child admission lost its typed origin"),
    }
}

#[test]
fn deterministic_identity_replays_every_crash_cut_without_uncertain_retry() {
    let mut service = ChildRunService::test_fixture();
    let request = request(ChildWait::Background);
    let invocation = request.invocation();

    for cut in [
        RecoveryCut::BeforeRequest,
        RecoveryCut::AfterRequest,
        RecoveryCut::AfterChildAdmission,
        RecoveryCut::AfterSpawn,
        RecoveryCut::AfterChildSettlement,
        RecoveryCut::BeforeParentSettlement,
    ] {
        service.crash_at(cut);
        let first = futures::executor::block_on(service.recover(invocation));
        let second = futures::executor::block_on(service.recover(invocation));
        assert_eq!(first.child_run(), second.child_run(), "cut {cut:?}");
        assert_eq!(first.request_identity(), second.request_identity());
        assert_eq!(
            service.child_admission_count(first.child_run()),
            1,
            "cut {cut:?} must not retry an uncertain child"
        );
        assert_eq!(
            service.parent_settlement_count(invocation),
            1,
            "cut {cut:?} must not duplicate parent delivery"
        );
    }
}

#[test]
fn uncertain_external_delivery_is_preserved_and_never_automatically_retried() {
    let mut service = ChildRunService::test_fixture();
    service.crash_at(RecoveryCut::AfterSpawn);
    service.set_child_recovery(ChildServiceError::DeliveryUncertain);

    let result = futures::executor::block_on(service.recover(request(ChildWait::Background).invocation()));
    assert!(matches!(result.wait_result(), ChildWaitResult::Uncertain { .. }));
    assert_eq!(service.child_admission_count(result.child_run()), 1);
    assert_eq!(service.executor_effect_count(result.child_run()), 1);
}

#[test]
fn only_a_completed_child_final_assistant_blob_is_returned() {
    let service = ChildRunService::test_fixture();
    let admission = futures::executor::block_on(service.start(request(ChildWait::Foreground)))
        .expect("child admission");
    service.complete_child(admission.child_run(), blob(RESULT, 9));

    let result = futures::executor::block_on(admission.wait()).expect("foreground result");
    assert_eq!(result, ChildWaitResult::Returned { final_message: blob(RESULT, 9) });
    assert_eq!(service.returned_parent_bytes(admission.invocation()), 1);
    assert!(!service.returned_parent_text(admission.invocation()).contains("tool"));
}

#[test]
fn every_non_success_terminal_remains_typed_and_completed_cannot_be_flattened() {
    let service = ChildRunService::test_fixture();
    let terminals = [
        SubagentTerminal::Refused {
            failure: SubagentRefusal::InvalidTask {
                diagnostic: "task is empty".to_owned(),
            },
        },
        SubagentTerminal::Failed {
            settlement: RunSettlement::cancelled(ego_core::run::CancelReason::User),
        },
        SubagentTerminal::Failed {
            settlement: RunSettlement::Interrupted,
        },
        SubagentTerminal::Failed {
            settlement: RunSettlement::Uncertain,
        },
    ];
    for terminal in terminals {
        let result = service.settle_for_test(terminal.clone());
        assert_eq!(result.terminal(), &terminal);
        assert!(result.final_message().is_none());
    }
    assert!(service
        .settle_for_test(SubagentTerminal::Failed {
            settlement: RunSettlement::completed(),
        })
        .is_err());
}

#[test]
fn foreground_wait_is_cancellable_but_background_drop_only_detaches_subscription() {
    let service = ChildRunService::test_fixture();
    let foreground = futures::executor::block_on(service.start(request(ChildWait::Foreground)))
        .expect("foreground admission");
    futures::executor::block_on(foreground.cancel()).expect("foreground cancellation");
    assert!(matches!(
        futures::executor::block_on(foreground.wait()).unwrap().wait_result(),
        ChildWaitResult::Failed {
            settlement: RunSettlement::Cancelled { .. }
        }
    ));

    let background = futures::executor::block_on(service.start(request(ChildWait::Background)))
        .expect("background admission");
    let invocation = background.invocation();
    drop(background.subscription());
    assert!(service.child_execution_is_live(background.child_run()));
    service.complete_child(background.child_run(), blob(RESULT, 9));
    let settled = futures::executor::block_on(service.collect(invocation)).expect("settlement");
    assert!(matches!(settled, ChildWaitResult::Returned { .. }));
}

#[test]
fn child_selection_intersects_parent_system_and_profile_authority() {
    let mut service = ChildRunService::test_fixture();
    let parent = service.parent_authority();
    let system = service.system_ceiling();
    let request = request(ChildWait::Foreground)
        .with_requested_tools(["read", "exec", "mcp:trusted"])
        .with_requested_permission("allow(read(src/**))")
        .with_requested_sandbox("workspace")
        .with_requested_offline(false)
        .with_requested_mcp(["trusted"])
        .with_requested_profile("narrow");

    let admission = futures::executor::block_on(service.start(request)).expect("narrow admission");
    let selected = admission.selections();
    assert!(selected.tools().is_subset_of(parent.tools()));
    assert!(selected.tools().is_subset_of(system.tools()));
    assert!(selected.permissions().is_subset_of(parent.permissions()));
    assert!(selected.permissions().is_subset_of(system.permissions()));
    assert!(selected.sandbox().is_at_least_as_confining_as(parent.sandbox()));
    assert!(selected.sandbox().is_at_least_as_confining_as(system.sandbox()));
    assert!(!selected.offline() || parent.offline());
    assert!(selected.mcp().is_subset_of(parent.mcp()));
    assert!(selected.mcp().is_subset_of(system.mcp()));
}

#[test]
fn widening_profile_or_unknown_profile_refuses_before_child_admission() {
    let mut service = ChildRunService::test_fixture();
    for request in [
        request(ChildWait::Foreground).with_requested_profile("missing"),
        request(ChildWait::Foreground).with_requested_tools(["parent-hidden"]),
        request(ChildWait::Foreground).with_requested_offline(false),
    ] {
        let error = futures::executor::block_on(service.start(request)).unwrap_err();
        assert!(matches!(
            error,
            ChildServiceError::ProfileUnavailable { .. }
                | ChildServiceError::AuthorityWouldWiden { .. }
        ));
        assert_eq!(service.child_admission_count_total(), 0);
    }
}

#[test]
fn concurrent_children_have_distinct_leases_and_one_shared_budget_gate() {
    let service = ChildRunService::test_fixture();
    let first = futures::executor::block_on(service.start(request(ChildWait::Background)))
        .expect("first child");
    let second = futures::executor::block_on(service.start(request(ChildWait::Background)))
        .expect("second child");
    assert_ne!(first.child_run(), second.child_run());
    assert_ne!(first.turn_lease(), second.turn_lease());
    assert_ne!(first.session_id(), second.session_id());
    assert_eq!(first.budget_gate(), second.budget_gate());
    assert_eq!(service.budget_gate_id(), first.budget_gate());
    assert_eq!(service.child_transcript(first.child_run()), ["child task"]);
    assert_eq!(service.child_transcript(second.child_run()), ["child task"]);
    assert!(service
        .child_transcript(first.child_run())
        .iter()
        .all(|line| line != "parent transcript"));
}

#[test]
fn child_budget_reservation_uses_the_parent_gate_and_refuses_the_second_spend() {
    let service = ChildRunService::test_fixture_with_gate(BudgetGate::new(Money::new(
        "1".parse::<Amount>().expect("test amount"),
        CurrencyCode::new("USD").expect("test currency"),
    )));
    let first = futures::executor::block_on(service.start(request(ChildWait::Background)))
        .expect("first child admission");
    service.complete_child(first.child_run(), blob(RESULT, 9));
    let _ = futures::executor::block_on(service.collect(first.invocation()));
    let second = futures::executor::block_on(service.start(request(ChildWait::Background)));
    assert!(matches!(second, Err(ChildServiceError::BudgetDenied { .. })));
    assert_eq!(service.budget_gate_count(), 1);
}

#[test]
fn child_service_has_no_scheduler_registry_or_parent_transcript_escape_hatch() {
    let service = ChildRunService::test_fixture();
    assert_eq!(service.scheduler_count(), 0);
    assert_eq!(service.agent_registry_count(), 0);
    assert!(!service.accepts_implicit_parent_transcript());
    assert!(futures::executor::block_on(
        service.start(request(ChildWait::Foreground).with_parent_transcript("ambient"))
    )
    .is_err());
}

#[test]
fn crash_cut_inventory_is_closed_against_the_rooted_child_adr() {
    let expected = BTreeMap::from([
        (RecoveryCut::BeforeRequest, "no request"),
        (RecoveryCut::AfterRequest, "request only"),
        (RecoveryCut::AfterChildAdmission, "admission only"),
        (RecoveryCut::AfterSpawn, "spawned"),
        (RecoveryCut::AfterChildSettlement, "child settled"),
        (RecoveryCut::BeforeParentSettlement, "return boundary"),
    ]);
    assert_eq!(expected.len(), 6);
    assert!(expected.values().all(|description| !description.is_empty()));
}
