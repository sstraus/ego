//! RED contract for the target-log-backed regression case compaction transition.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ego_core::compact::{
    CompactionError, CompactionFuture, CompactionPreparation, CompactionTargetError,
    CompactionTargetFacts, CompactionTargetInspection, CompactionTargetPort,
    CompactionTransitionError, CompactionTransitionRequest, CompactionTransitionService,
    PreparedCompaction,
};
use ego_core::event::id::{Seq, SessionId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::run::RequestId;
use ego_core::session::TargetPublication;

#[derive(Clone, Copy, Default)]
struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(68_000)
    }
}

fn request(value: u128) -> CompactionTransitionRequest {
    CompactionTransitionRequest {
        request_id: RequestId::from_uuid(uuid::Uuid::from_u128(value)),
        source: SessionId::new(),
    }
}

struct Preparation {
    source: SessionId,
    calls: Arc<Mutex<usize>>,
}

impl CompactionPreparation for Preparation {
    fn source(&self) -> SessionId {
        self.source
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> CompactionFuture<'a, Result<PreparedCompaction, CompactionError>> {
        *self.calls.lock().expect("preparation calls") += 1;
        Box::pin(async {
            Err(CompactionError::Internal(
                "preparation fixture is intentionally unavailable".to_owned(),
            ))
        })
    }
}

struct Targets {
    inspections: Mutex<VecDeque<Result<CompactionTargetInspection, CompactionTargetError>>>,
    confirms: Mutex<Vec<CompactionTargetFacts>>,
    confirmation_outcomes: Mutex<VecDeque<Result<TargetPublication, CompactionTargetError>>>,
    publications: Mutex<Vec<TargetPublication>>,
    published_seeds: Mutex<Vec<ego_core::session::TargetSeed>>,
}

impl Targets {
    fn with_inspections(
        inspections: impl IntoIterator<Item = Result<CompactionTargetInspection, CompactionTargetError>>,
    ) -> Self {
        Self {
            inspections: Mutex::new(inspections.into_iter().collect()),
            confirms: Mutex::new(Vec::new()),
            confirmation_outcomes: Mutex::new(VecDeque::new()),
            publications: Mutex::new(Vec::new()),
            published_seeds: Mutex::new(Vec::new()),
        }
    }

    fn with_confirmation(
        inspections: impl IntoIterator<Item = Result<CompactionTargetInspection, CompactionTargetError>>,
        outcomes: impl IntoIterator<Item = Result<TargetPublication, CompactionTargetError>>,
    ) -> Self {
        Self {
            inspections: Mutex::new(inspections.into_iter().collect()),
            confirms: Mutex::new(Vec::new()),
            confirmation_outcomes: Mutex::new(outcomes.into_iter().collect()),
            publications: Mutex::new(Vec::new()),
            published_seeds: Mutex::new(Vec::new()),
        }
    }

    fn published_seeds(&self) -> Vec<ego_core::session::TargetSeed> {
        self.published_seeds
            .lock()
            .expect("published seeds")
            .clone()
    }

    fn confirmations(&self) -> Vec<CompactionTargetFacts> {
        self.confirms.lock().expect("target confirmations").clone()
    }
}

impl CompactionTargetPort for Targets {
    fn inspect(
        &self,
        _target: SessionId,
    ) -> Result<CompactionTargetInspection, CompactionTargetError> {
        self.inspections
            .lock()
            .expect("target inspections")
            .pop_front()
            .unwrap_or_else(|| {
                Err(CompactionTargetError {
                    diagnostic: "unexpected target inspection".to_owned(),
                })
            })
    }

    fn publish(
        &self,
        _target: SessionId,
        seed: &ego_core::session::TargetSeed,
    ) -> TargetPublication {
        self.published_seeds
            .lock()
            .expect("published seeds")
            .push(seed.clone());
        self.publications
            .lock()
            .expect("target publications")
            .pop()
            .unwrap_or(TargetPublication::NotPublished {
                diagnostic: "publication fixture is intentionally unavailable".to_owned(),
            })
    }

    fn confirm(
        &self,
        _target: SessionId,
        expected: CompactionTargetFacts,
    ) -> Result<TargetPublication, CompactionTargetError> {
        self.confirms
            .lock()
            .expect("target confirmations")
            .push(expected);
        self.confirmation_outcomes
            .lock()
            .expect("confirmation outcomes")
            .pop_front()
            .unwrap_or_else(|| Ok(TargetPublication::PublishedDurably { diagnostic: None }))
    }
}

fn facts(source: SessionId) -> CompactionTargetFacts {
    CompactionTargetFacts {
        source,
        source_seq: Seq::ZERO,
        prompt_revision: 1,
    }
}

#[test]
fn request_id_deterministically_identifies_target_and_successor_start() {
    let request = request(6801);
    let target = RequestId::session_id(request.request_id);
    let targets = Targets::with_confirmation(
        [Ok(CompactionTargetInspection::Compaction(facts(
            request.source,
        )))],
        [Ok(TargetPublication::PublishedDurably { diagnostic: None })],
    );
    let calls = Arc::new(Mutex::new(0));
    let mut preparation = Preparation {
        source: request.source,
        calls: Arc::clone(&calls),
    };

    let receipt = futures::executor::block_on(
        CompactionTransitionService::new(FixedClock).compact(request, &mut preparation, &targets),
    )
    .expect("matching target confirms");
    assert_eq!(receipt.target, target);
    assert_eq!(receipt.successor_start_request_id, request.request_id);
    assert_eq!(*calls.lock().expect("preparation calls"), 0);
    assert!(targets.published_seeds().is_empty());
    assert_eq!(targets.confirmations().len(), 1);
}

#[test]
fn matching_target_confirmation_returns_uncertainty_without_retry_permission() {
    let request = request(6807);
    let mut preparation = Preparation {
        source: request.source,
        calls: Arc::new(Mutex::new(0)),
    };
    let targets = Targets::with_confirmation(
        [Ok(CompactionTargetInspection::Compaction(facts(
            request.source,
        )))],
        [Ok(TargetPublication::PublishedDurabilityUncertain {
            diagnostic: "parent barrier failed".to_owned(),
        })],
    );
    let receipt = futures::executor::block_on(
        CompactionTransitionService::new(FixedClock).compact(request, &mut preparation, &targets),
    )
    .expect("published uncertainty remains an authoritative receipt");
    assert!(matches!(
        receipt.publication,
        TargetPublication::PublishedDurabilityUncertain { .. }
    ));
}

#[test]
fn source_mismatch_is_rejected_before_target_inspection_or_preparation() {
    let request = request(6802);
    let calls = Arc::new(Mutex::new(0));
    let mut preparation = Preparation {
        source: SessionId::new(),
        calls: Arc::clone(&calls),
    };
    let targets = Targets::with_inspections([]);
    let result = futures::executor::block_on(CompactionTransitionService::new(FixedClock).compact(
        request,
        &mut preparation,
        &targets,
    ));
    assert!(matches!(
        result,
        Err(CompactionTransitionError::SourceMismatch { .. })
    ));
    assert_eq!(*calls.lock().expect("preparation calls"), 0);
}

#[test]
fn occupied_target_is_an_identity_collision_without_preparation() {
    let request = request(6803);
    let calls = Arc::new(Mutex::new(0));
    let mut preparation = Preparation {
        source: request.source,
        calls: Arc::clone(&calls),
    };
    let targets = Targets::with_inspections([Ok(CompactionTargetInspection::Occupied)]);
    let result = futures::executor::block_on(CompactionTransitionService::new(FixedClock).compact(
        request,
        &mut preparation,
        &targets,
    ));
    assert!(matches!(
        result,
        Err(CompactionTransitionError::IdentityCollision { .. })
    ));
    assert_eq!(*calls.lock().expect("preparation calls"), 0);
}

#[test]
fn missing_target_prepares_once_and_exposes_the_preparation_failure() {
    let request = request(6804);
    let calls = Arc::new(Mutex::new(0));
    let mut preparation = Preparation {
        source: request.source,
        calls: Arc::clone(&calls),
    };
    let targets = Targets::with_inspections([Ok(CompactionTargetInspection::Missing)]);
    let result = futures::executor::block_on(CompactionTransitionService::new(FixedClock).compact(
        request,
        &mut preparation,
        &targets,
    ));
    assert!(matches!(
        result,
        Err(CompactionTransitionError::Preparation(_))
    ));
    assert_eq!(*calls.lock().expect("preparation calls"), 1);
}

#[test]
fn matching_target_confirmation_returns_a_durable_receipt() {
    let request = request(6805);
    let mut preparation = Preparation {
        source: request.source,
        calls: Arc::new(Mutex::new(0)),
    };
    let targets = Targets::with_inspections([Ok(CompactionTargetInspection::Compaction(facts(
        request.source,
    )))]);
    let receipt = futures::executor::block_on(
        CompactionTransitionService::new(FixedClock).compact(request, &mut preparation, &targets),
    )
    .expect("matching target is authoritative");
    assert_eq!(
        receipt.publication,
        TargetPublication::PublishedDurably { diagnostic: None }
    );
}

#[test]
fn target_inspection_failure_is_not_reinterpreted_as_preparation_work() {
    let request = request(6806);
    let calls = Arc::new(Mutex::new(0));
    let mut preparation = Preparation {
        source: request.source,
        calls: Arc::clone(&calls),
    };
    let targets = Targets::with_inspections([Err(CompactionTargetError {
        diagnostic: "corrupt target".to_owned(),
    })]);
    let result = futures::executor::block_on(CompactionTransitionService::new(FixedClock).compact(
        request,
        &mut preparation,
        &targets,
    ));
    assert!(matches!(result, Err(CompactionTransitionError::Target(_))));
    assert_eq!(*calls.lock().expect("preparation calls"), 0);
}
