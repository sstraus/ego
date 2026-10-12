//! Re-admission against the real session writer and executor.
use super::regression_case_tests::{catalogue, run_child};
use ego_core::event::id::ItemId;
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::permit::{HumanApprover, PermissionDecision, PermissionRequest};
use ego_core::run::*;
use ego_core::sink::{EventSink, NullSink};
use ego_core::store::run_artifact::RunArtifactStore;
use ego_core::store::session_store::{SessionEntry, SessionStore};
use futures::StreamExt;
use futures::future::BoxFuture;
use std::sync::Arc;

struct Sinks;
impl EventSinkFactory for Sinks {
    fn create(&self, _: &ModelCapabilities) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }
}

#[derive(Debug, Default)]
struct HeldHuman {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl HumanApprover for HeldHuman {
    fn decide<'a>(
        &'a self,
        _: ItemId,
        _: &'a PermissionRequest,
    ) -> BoxFuture<'a, PermissionDecision> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            PermissionDecision::Allow
        })
    }
}

// Catches: overlapping real continuations append decisions from competing stale Journals.
#[test]
fn overlapping_readmission_permissions_keep_the_session_event_sequence_unique() {
    if std::env::var("EGO_REGRESSION_CHILD").as_deref() != Ok("overlapping_readmission") {
        run_child("overlapping_readmission", Some("off"));
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let owner = tempfile::tempdir().unwrap();
            let workspace = owner.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let root = RunRoot::new(workspace.clone()).unwrap();
            let store = Arc::new(ego_tools::store::JsonlStore::new(
                owner.path().join("sessions"),
                workspace,
                Box::new(ego_tools::store::OsIo),
            ));
            let artifacts: Arc<dyn RunArtifactStore> = store.clone();
            let (executor, _) = super::CliRunExecutor::for_mcp(
                root.clone(),
                None,
                store.clone(),
                artifacts,
                std::num::NonZeroUsize::new(2).unwrap(),
                Arc::new(ego_core::evidence::EvidenceAdmissionService::new()),
                Some(&catalogue(None)),
            )
            .unwrap();
            let controller = Arc::new(RunController::new(
                store.clone(),
                Arc::new(executor),
                Arc::new(super::SystemClock),
                Arc::new(crate::host_ports::UuidIds),
                RunRetention::new(std::num::NonZeroUsize::new(8).unwrap()).unwrap(),
            ));
            let request = || RequestId::from_uuid(uuid::Uuid::now_v7());
            let initial = request();
            let session = RequestId::session_id(initial);
            let old = ego_core::permit::ExecObservation::new(
                ego_core::permit::BackendIdentity::new("null", "1"),
                ego_core::permit::ExecCaps::empty(),
                ego_core::permit::PolicyDigest::try_from("1".repeat(64)).unwrap(),
            );
            let first = controller
                .start(
                    StartRequest::new(
                        session,
                        root,
                        initial,
                        RequestDigest::of(&"initial").unwrap(),
                        RunSelections::new(None, None, None, None, None)
                            .with_sandbox_observation(ego_core::permit::Confinement::Off, old),
                        "initial",
                    ),
                    RunExecutionBindings::new(Arc::new(Sinks), None),
                )
                .await
                .unwrap();
            let RunCommandOutcome::Admitted(first) = first else {
                panic!("initial admission")
            };
            let run = first.run_id();
            drop(first);
            // Persist a completed turn using the actual admission's lease and selections;
            // no provider execution is needed to test continuation admission.
            let entries: Vec<_> = SessionStore::read(store.as_ref(), &session)
                .unwrap()
                .collect()
                .await;
            let admission = entries
                .into_iter()
                .find_map(|entry| match entry.unwrap() {
                    SessionEntry::RunTransaction(tx) => Some(tx),
                    _ => None,
                })
                .unwrap();
            let RunCommand::CommandAdmitted { lease, .. } = admission.command() else {
                panic!("admission transaction")
            };
            let mut settled = serde_json::to_value(&admission).unwrap();
            settled["revision"] = serde_json::to_value(admission.revision().next()).unwrap();
            settled["status"] = serde_json::to_value(RunStatus::Settled).unwrap();
            settled["command"] = serde_json::to_value(RunCommand::CommandSettled {
                lease: *lease,
                settlement: RunSettlement::Completed,
            })
            .unwrap();
            let settled: RunTransaction = serde_json::from_value(settled).unwrap();
            let revision = settled.revision();
            assert!(matches!(
                store
                    .compare_and_append_run(&session, Some(admission.revision()), settled)
                    .unwrap(),
                RunAppend::Committed { .. }
            ));
            let human = Arc::new(HeldHuman::default());
            let left_controller = controller.clone();
            let left_human = human.clone();
            let left_request = request();
            let left = tokio::spawn(async move {
                left_controller
                    .continue_run(
                        ContinueRequest::new(
                            run,
                            left_request,
                            RequestDigest::of(&"left").unwrap(),
                            revision,
                            "left",
                        ),
                        RunExecutionBindings::new(Arc::new(Sinks), None).with_human(left_human),
                    )
                    .await
            });
            human.entered.notified().await;
            let right_controller = controller.clone();
            let right_human = human.clone();
            let right_request = request();
            let mut right = tokio::spawn(async move {
                right_controller
                    .continue_run(
                        ContinueRequest::new(
                            run,
                            right_request,
                            RequestDigest::of(&"right").unwrap(),
                            revision,
                            "right",
                        ),
                        RunExecutionBindings::new(Arc::new(Sinks), None).with_human(right_human),
                    )
                    .await
            });
            let right_result = tokio::select! {
                result = &mut right => Some(result.unwrap()),
                _ = human.entered.notified() => None,
            };
            human.release.notify_waiters();
            let left_result = left.await.unwrap();
            let right_result = match right_result {
                Some(result) => result,
                None => right.await.unwrap(),
            };
            assert_ne!(
                left_result.is_ok(),
                right_result.is_ok(),
                "exactly one continuation must admit"
            );
            let events: Vec<_> = SessionStore::read(store.as_ref(), &session)
                .unwrap()
                .filter_map(|entry| async move {
                    match entry.unwrap() {
                        SessionEntry::Event(event) => Some(event),
                        _ => None,
                    }
                })
                .collect()
                .await;
            assert_eq!(
                events.len(),
                3,
                "real boundary must persist observation, request and decision once"
            );
            assert!(matches!(
                events[0].kind,
                ego_core::event::domain::EventKind::ExecutionObserved { .. }
            ));
            assert!(matches!(
                events[1].kind,
                ego_core::event::domain::EventKind::PermissionRequested { .. }
            ));
            assert!(matches!(
                events[2].kind,
                ego_core::event::domain::EventKind::PermissionDecided {
                    allowed: Some(true),
                    by: Some(ego_core::permit::Authority::Party),
                    ..
                }
            ));
            assert_eq!(events[2].env.parent, Some(events[1].env.id));
            for pair in events.windows(2) {
                assert!(
                    pair[0].env.seq < pair[1].env.seq,
                    "overlapping re-admission corrupted durable event order: {:?} then {:?}",
                    pair[0].env.seq,
                    pair[1].env.seq
                );
            }
        });
}
