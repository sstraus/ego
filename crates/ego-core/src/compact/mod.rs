//! Prompted compaction into a clean checkpoint-seeded session.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::Serialize;

use crate::budget::BudgetDenied;
use crate::event::domain::{CancelCause, GenerationRole};
use crate::event::id::{Seq, SessionId};
use crate::event::time::{Clock, Timestamp};
use crate::generation_model_source::GenerationModelError;
use crate::generator::capabilities::CapabilityGap;
use crate::generator::port::RequestExpressibilityError;
use crate::generator::types::{GenRequest, HistoryError, StopReason};
use crate::goal::GoalRecord;
use crate::run::RequestId;
use crate::session::{TargetPublication, TargetSeed};
use crate::store::session_store::StoreError;

pub const COMPACTION_PROMPT_REVISION: u32 = 2;

pub const COMPACTION_PROMPT: &str = r###"Produce one self-contained context checkpoint for continuing this work in a
new session. Preserve every material fact from the complete context above, but
do not continue the task, address the operator, call tools, or invent missing
information.

Output format is a strict parser contract, not a suggestion. Start immediately
with # Objective, with no preamble or outer code fence. Use exactly these eight
level-one headings, each once, in this order:
# Objective
# Binding constraints and preferences
# Completed work and evidence
# Work in progress
# Decisions and rationale
# Blockers and uncertainty
# Ordered next steps
# Critical references

Do not emit any other line that starts with "# ", even inside fenced code or
quoted source excerpts. For copied Markdown headings, shell comments, or error
lines that start with "# ", indent the line by four spaces or prefix it with
"> " so it is body text, not a checkpoint heading. Use level-two headings
("## ") for any additional sections. Do not repeat checkpoint headings when
quoting an earlier checkpoint; merge its facts into the sections above.

Include exact file paths, symbols, commands, results, identifiers, and error
details when they matter. Reconcile repeated or superseded information. If an
earlier context checkpoint is present, preserve every still-relevant fact from
it. Return only the checkpoint."###;

const HEADINGS: [&str; 8] = [
    "# Objective",
    "# Binding constraints and preferences",
    "# Completed work and evidence",
    "# Work in progress",
    "# Decisions and rationale",
    "# Blockers and uncertainty",
    "# Ordered next steps",
    "# Critical references",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint(String);

impl Checkpoint {
    pub fn parse(text: impl Into<String>) -> Result<Self, CheckpointError> {
        let text = text.into();
        let text = text.trim().to_string();
        if !text.starts_with(HEADINGS[0]) {
            return Err(CheckpointError::TextBeforeObjective);
        }

        let mut seen = Vec::new();
        for line in text.lines() {
            if !line.starts_with("# ") {
                continue;
            }
            let Some(index) = HEADINGS.iter().position(|heading| *heading == line) else {
                return Err(CheckpointError::UnknownHeading(line.to_string()));
            };
            if seen.contains(&index) {
                return Err(CheckpointError::DuplicateHeading(line.to_string()));
            }
            seen.push(index);
        }
        if seen.len() != HEADINGS.len() {
            let missing = HEADINGS
                .iter()
                .enumerate()
                .find(|(index, _)| !seen.contains(index))
                .map(|(_, heading)| (*heading).to_string())
                .expect("fewer than eight unique headings leaves one missing");
            return Err(CheckpointError::MissingHeading(missing));
        }
        if seen != (0..HEADINGS.len()).collect::<Vec<_>>() {
            return Err(CheckpointError::OutOfOrderHeading);
        }

        let objective_has_text = text
            .lines()
            .skip(1)
            .take_while(|line| *line != HEADINGS[1])
            .any(|line| !line.trim().is_empty());
        if !objective_has_text {
            return Err(CheckpointError::EmptyObjective);
        }
        Ok(Self(text))
    }

    pub fn text(&self) -> &str {
        &self.0
    }

    pub fn into_text(self) -> String {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointError {
    TextBeforeObjective,
    MissingHeading(String),
    DuplicateHeading(String),
    UnknownHeading(String),
    OutOfOrderHeading,
    EmptyObjective,
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TextBeforeObjective => {
                formatter.write_str("checkpoint must start with # Objective")
            }
            Self::MissingHeading(heading) => write!(formatter, "checkpoint is missing {heading}"),
            Self::DuplicateHeading(heading) => write!(formatter, "checkpoint repeats {heading}"),
            Self::UnknownHeading(heading) => {
                write!(formatter, "checkpoint contains unknown heading {heading}")
            }
            Self::OutOfOrderHeading => formatter.write_str("checkpoint headings are out of order"),
            Self::EmptyObjective => formatter.write_str("checkpoint objective is empty"),
        }
    }
}

impl std::error::Error for CheckpointError {}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedCompaction {
    pub(crate) source: SessionId,
    pub(crate) source_seq: Seq,
    pub(crate) checkpoint: Checkpoint,
    pub(crate) baseline: GenRequest,
    pub(crate) title: Option<String>,
    pub(crate) goal: Option<GoalRecord>,
}

impl PreparedCompaction {
    pub fn source(&self) -> SessionId {
        self.source
    }

    pub fn source_seq(&self) -> Seq {
        self.source_seq
    }

    pub fn prompt_revision(&self) -> u32 {
        COMPACTION_PROMPT_REVISION
    }

    pub fn checkpoint(&self) -> &Checkpoint {
        &self.checkpoint
    }

    pub fn baseline(&self) -> &GenRequest {
        &self.baseline
    }

    pub fn target_seed(&self, at: Timestamp) -> TargetSeed {
        TargetSeed::compaction(
            self.source,
            self.source_seq,
            COMPACTION_PROMPT_REVISION,
            self.checkpoint.text().to_string(),
            at,
        )
        .with_title(self.title.clone())
        .with_goal(self.goal.as_ref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionBusy {
    AcceptedPrompt,
    PendingQuestion,
    LiveAttempt,
}

#[derive(Debug)]
pub enum CompactionError {
    NotIdle(CompactionBusy),
    History(HistoryError),
    Resolution {
        role: GenerationRole,
        error: GenerationModelError,
    },
    Capability {
        role: GenerationRole,
        gaps: Vec<CapabilityGap>,
    },
    Expressibility {
        role: GenerationRole,
        error: RequestExpressibilityError,
    },
    Budget(BudgetDenied),
    Cancelled(CancelCause),
    UnexpectedStop(StopReason),
    EmptyCheckpoint,
    InvalidCheckpoint(CheckpointError),
    Store(StoreError),
    Internal(String),
}

impl fmt::Display for CompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotIdle(reason) => write!(formatter, "compaction is not idle: {reason:?}"),
            Self::History(error) => write!(formatter, "source history cannot be rebuilt: {error}"),
            Self::Resolution { role, error } => {
                write!(formatter, "{role:?} resolution failed: {error}")
            }
            Self::Capability { role, gaps } => {
                write!(formatter, "{role:?} request has capability gaps: {gaps:?}")
            }
            Self::Expressibility { role, error } => {
                write!(formatter, "{role:?} request cannot be encoded: {error}")
            }
            Self::Budget(error) => write!(formatter, "{error}"),
            Self::Cancelled(cause) => write!(formatter, "compaction was cancelled by {cause:?}"),
            Self::UnexpectedStop(stop) => write!(formatter, "compaction stopped with {stop:?}"),
            Self::EmptyCheckpoint => formatter.write_str("compaction returned an empty checkpoint"),
            Self::InvalidCheckpoint(error) => write!(formatter, "{error}"),
            Self::Store(error) => write!(formatter, "{error}"),
            Self::Internal(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for CompactionError {}

impl From<StoreError> for CompactionError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// One caller-owned identity for a deterministic compaction transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionTransitionRequest {
    pub request_id: RequestId,
    pub source: SessionId,
}

/// Stable facts projected from the authoritative sequence-1 compaction seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionTargetFacts {
    pub source: SessionId,
    pub source_seq: Seq,
    pub prompt_revision: u32,
}

/// What currently occupies the deterministic target identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTargetInspection {
    Missing,
    Compaction(CompactionTargetFacts),
    Occupied,
}

/// Failure to inspect or confirm a rooted compaction target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionTargetError {
    pub diagnostic: String,
}

impl fmt::Display for CompactionTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.diagnostic)
    }
}

impl std::error::Error for CompactionTargetError {}

/// Consumer-owned target lifecycle around the existing atomic publisher.
pub trait CompactionTargetPort: Send + Sync {
    fn inspect(
        &self,
        target: SessionId,
    ) -> Result<CompactionTargetInspection, CompactionTargetError>;

    fn publish(&self, target: SessionId, seed: &TargetSeed) -> TargetPublication;

    fn confirm(
        &self,
        target: SessionId,
        expected: CompactionTargetFacts,
    ) -> Result<TargetPublication, CompactionTargetError>;
}

pub type CompactionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Host-facing application port for one complete compaction transition.
pub trait CompactionCommand: Send + Sync {
    fn compact<'a>(
        &'a self,
        request: CompactionTransitionRequest,
    ) -> CompactionFuture<'a, Result<CompactionReceipt, CompactionTransitionError>>;
}

/// The source-owned effect that prepares one canonical checkpoint seed.
pub trait CompactionPreparation: Send {
    fn source(&self) -> SessionId;

    fn prepare<'a>(
        &'a mut self,
    ) -> CompactionFuture<'a, Result<PreparedCompaction, CompactionError>>;
}

/// Stable receipt for both a new publication and an idempotent confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompactionReceipt {
    pub source: SessionId,
    pub source_seq: Seq,
    pub target: SessionId,
    pub publication: TargetPublication,
    pub successor_start_request_id: RequestId,
}

/// Closed application failures before a published target can be selected.
#[derive(Debug)]
pub enum CompactionTransitionError {
    SourceMismatch {
        requested: SessionId,
        actual: SessionId,
    },
    Target(CompactionTargetError),
    IdentityCollision {
        request_id: RequestId,
        target: SessionId,
    },
    Preparation(CompactionError),
    NotPublished {
        diagnostic: String,
    },
}

impl fmt::Display for CompactionTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceMismatch { requested, actual } => write!(
                formatter,
                "compaction source {actual} does not match requested source {requested}"
            ),
            Self::Target(error) => write!(formatter, "compaction target failed: {error}"),
            Self::IdentityCollision { request_id, target } => write!(
                formatter,
                "compaction request {request_id:?} collides with target {target}"
            ),
            Self::Preparation(error) => write!(formatter, "compaction preparation failed: {error}"),
            Self::NotPublished { diagnostic } => {
                write!(
                    formatter,
                    "compaction target was not published: {diagnostic}"
                )
            }
        }
    }
}

impl std::error::Error for CompactionTransitionError {}

/// The sole application owner of deterministic compaction idempotency.
pub struct CompactionTransitionService<C> {
    clock: C,
}

impl<C> CompactionTransitionService<C> {
    pub const fn new(clock: C) -> Self {
        Self { clock }
    }
}

impl<C: Clock> CompactionTransitionService<C> {
    pub async fn compact(
        &self,
        request: CompactionTransitionRequest,
        source: &mut dyn CompactionPreparation,
        targets: &dyn CompactionTargetPort,
    ) -> Result<CompactionReceipt, CompactionTransitionError> {
        let actual = source.source();
        if actual != request.source {
            return Err(CompactionTransitionError::SourceMismatch {
                requested: request.source,
                actual,
            });
        }

        let target = RequestId::session_id(request.request_id);
        match targets
            .inspect(target)
            .map_err(CompactionTransitionError::Target)?
        {
            CompactionTargetInspection::Missing => {
                let prepared = source
                    .prepare()
                    .await
                    .map_err(CompactionTransitionError::Preparation)?;
                if prepared.source() != request.source {
                    return Err(CompactionTransitionError::SourceMismatch {
                        requested: request.source,
                        actual: prepared.source(),
                    });
                }
                let expected = CompactionTargetFacts {
                    source: prepared.source(),
                    source_seq: prepared.source_seq(),
                    prompt_revision: prepared.prompt_revision(),
                };
                let seed = prepared.target_seed(self.clock.now());
                match targets.publish(target, &seed) {
                    TargetPublication::NotPublished { diagnostic } => {
                        match targets
                            .inspect(target)
                            .map_err(CompactionTransitionError::Target)?
                        {
                            CompactionTargetInspection::Compaction(actual)
                                if actual == expected =>
                            {
                                let publication = targets
                                    .confirm(target, expected)
                                    .map_err(CompactionTransitionError::Target)?;
                                receipt(request, target, expected, publication)
                            }
                            CompactionTargetInspection::Missing => {
                                Err(CompactionTransitionError::NotPublished { diagnostic })
                            }
                            CompactionTargetInspection::Compaction(_)
                            | CompactionTargetInspection::Occupied => {
                                Err(CompactionTransitionError::IdentityCollision {
                                    request_id: request.request_id,
                                    target,
                                })
                            }
                        }
                    }
                    publication => receipt(request, target, expected, publication),
                }
            }
            CompactionTargetInspection::Compaction(facts)
                if facts.source == request.source
                    && facts.prompt_revision == COMPACTION_PROMPT_REVISION =>
            {
                let publication = targets
                    .confirm(target, facts)
                    .map_err(CompactionTransitionError::Target)?;
                receipt(request, target, facts, publication)
            }
            CompactionTargetInspection::Compaction(_) | CompactionTargetInspection::Occupied => {
                Err(CompactionTransitionError::IdentityCollision {
                    request_id: request.request_id,
                    target,
                })
            }
        }
    }
}

fn receipt(
    request: CompactionTransitionRequest,
    target: SessionId,
    facts: CompactionTargetFacts,
    publication: TargetPublication,
) -> Result<CompactionReceipt, CompactionTransitionError> {
    match publication {
        TargetPublication::NotPublished { diagnostic } => {
            Err(CompactionTransitionError::NotPublished { diagnostic })
        }
        publication => Ok(CompactionReceipt {
            source: facts.source,
            source_seq: facts.source_seq,
            target,
            publication,
            successor_start_request_id: request.request_id,
        }),
    }
}

#[derive(Serialize)]
struct CheckpointFrame<'a> {
    kind: &'static str,
    revision: u32,
    text: &'a str,
}

pub(crate) fn checkpoint_frame(text: &str) -> String {
    serde_json::to_string(&CheckpointFrame {
        kind: "context_checkpoint",
        revision: COMPACTION_PROMPT_REVISION,
        text,
    })
    .expect("the checkpoint frame contains only serializable fields")
}

#[cfg(test)]
mod transition_contract_tests {
    use super::*;
    use crate::event::domain::EventKind;
    use crate::event::id::SessionId;
    use crate::event::time::Clock;
    use crate::generator::types::{Conversation, Message, Role, ThinkingLevel};
    use crate::run::RequestId;
    use crate::session::{SessionLineage, TargetPublication};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Copy, Default)]
    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_millis(68_000)
        }
    }

    struct Preparation {
        source: SessionId,
        prepared: Option<PreparedCompaction>,
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
            Box::pin(std::future::ready(self.prepared.take().ok_or_else(|| {
                CompactionError::Internal("preparation was unexpectedly repeated".to_owned())
            })))
        }
    }

    struct Targets {
        inspections: Mutex<VecDeque<Result<CompactionTargetInspection, CompactionTargetError>>>,
        publications: Mutex<VecDeque<TargetPublication>>,
        published: Mutex<Vec<TargetSeed>>,
        confirmations: Mutex<Vec<CompactionTargetFacts>>,
        confirmed: Mutex<VecDeque<Result<TargetPublication, CompactionTargetError>>>,
    }

    impl Targets {
        fn new(
            inspections: impl IntoIterator<
                Item = Result<CompactionTargetInspection, CompactionTargetError>,
            >,
            confirmed: impl IntoIterator<Item = Result<TargetPublication, CompactionTargetError>>,
        ) -> Self {
            Self {
                inspections: Mutex::new(inspections.into_iter().collect()),
                publications: Mutex::new(VecDeque::new()),
                published: Mutex::new(Vec::new()),
                confirmations: Mutex::new(Vec::new()),
                confirmed: Mutex::new(confirmed.into_iter().collect()),
            }
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

        fn publish(&self, _target: SessionId, seed: &TargetSeed) -> TargetPublication {
            self.published
                .lock()
                .expect("published seeds")
                .push(seed.clone());
            self.publications
                .lock()
                .expect("target publications")
                .pop_front()
                .unwrap_or(TargetPublication::NotPublished {
                    diagnostic: "unexpected target publication".to_owned(),
                })
        }

        fn confirm(
            &self,
            _target: SessionId,
            expected: CompactionTargetFacts,
        ) -> Result<TargetPublication, CompactionTargetError> {
            self.confirmations
                .lock()
                .expect("target confirmations")
                .push(expected);
            self.confirmed
                .lock()
                .expect("confirmed publications")
                .pop_front()
                .unwrap_or(Ok(TargetPublication::PublishedDurably { diagnostic: None }))
        }
    }

    fn checkpoint() -> Checkpoint {
        Checkpoint::parse(
            "# Objective\nContinue.\n# Binding constraints and preferences\nPreserve.\n# Completed work and evidence\nFixture.\n# Work in progress\nTransition.\n# Decisions and rationale\nTyped.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nTest.\n# Critical references\nADR.",
        )
        .expect("valid checkpoint")
    }

    fn prepared(source: SessionId) -> PreparedCompaction {
        PreparedCompaction {
            source,
            source_seq: Seq::ZERO,
            checkpoint: checkpoint(),
            baseline: GenRequest {
                cache: None,
                model: "model/compaction".to_owned(),
                messages: Conversation::new(vec![Message::text(Role::User, "checkpoint")])
                    .expect("baseline conversation"),
                tools: Vec::new(),
                thinking: ThinkingLevel::Low,
                max_output_tokens: Some(256),
                parallel_tool_calls: false,
            },
            title: None,
            goal: None,
        }
    }

    fn request(source: SessionId, value: u128) -> CompactionTransitionRequest {
        CompactionTransitionRequest {
            request_id: RequestId::from_uuid(uuid::Uuid::from_u128(value)),
            source,
        }
    }

    fn facts(source: SessionId) -> CompactionTargetFacts {
        CompactionTargetFacts {
            source,
            source_seq: Seq::ZERO,
            prompt_revision: COMPACTION_PROMPT_REVISION,
        }
    }

    #[test]
    fn missing_target_success_publishes_the_exact_prepared_compaction_seed() {
        let source = SessionId::new();
        let request = request(source, 68_801);
        let calls = Arc::new(Mutex::new(0));
        let mut preparation = Preparation {
            source,
            prepared: Some(prepared(source)),
            calls: Arc::clone(&calls),
        };
        let targets = Targets::new([Ok(CompactionTargetInspection::Missing)], []);
        targets
            .publications
            .lock()
            .expect("target publications")
            .push_back(TargetPublication::PublishedDurably { diagnostic: None });

        let receipt =
            futures::executor::block_on(CompactionTransitionService::new(FixedClock).compact(
                request,
                &mut preparation,
                &targets,
            ))
            .expect("prepared compaction publishes");
        assert_eq!(receipt.target, RequestId::session_id(request.request_id));
        assert_eq!(*calls.lock().expect("preparation calls"), 1);
        let published = targets.published.lock().expect("published seeds");
        assert_eq!(published.len(), 1);
        assert_eq!(
            published[0].lineage(),
            SessionLineage::Compaction {
                source,
                source_seq: Seq::ZERO,
                prompt_revision: COMPACTION_PROMPT_REVISION,
            }
        );
        assert!(matches!(
            published[0].event().kind,
            EventKind::CompactionSeeded {
                source: actual,
                source_seq: Seq::ZERO,
                prompt_revision: COMPACTION_PROMPT_REVISION,
                ..
            } if actual == source
        ));
    }

    #[test]
    fn missing_target_returns_all_publication_outcomes_without_repeating_preparation() {
        for publication in [
            TargetPublication::NotPublished {
                diagnostic: "not published".to_owned(),
            },
            TargetPublication::PublishedDurably { diagnostic: None },
            TargetPublication::PublishedDurabilityUncertain {
                diagnostic: "barrier uncertain".to_owned(),
            },
        ] {
            let source = SessionId::new();
            let request = request(source, 68_802);
            let calls = Arc::new(Mutex::new(0));
            let mut preparation = Preparation {
                source,
                prepared: Some(prepared(source)),
                calls: Arc::clone(&calls),
            };
            let inspections = if matches!(&publication, &TargetPublication::NotPublished { .. }) {
                vec![
                    Ok(CompactionTargetInspection::Missing),
                    Ok(CompactionTargetInspection::Missing),
                ]
            } else {
                vec![Ok(CompactionTargetInspection::Missing)]
            };
            let targets = Targets::new(inspections, []);
            targets
                .publications
                .lock()
                .expect("target publications")
                .push_back(publication.clone());
            let result =
                futures::executor::block_on(CompactionTransitionService::new(FixedClock).compact(
                    request,
                    &mut preparation,
                    &targets,
                ));
            match publication {
                TargetPublication::NotPublished { .. } => assert!(matches!(
                    result,
                    Err(CompactionTransitionError::NotPublished { .. })
                )),
                TargetPublication::PublishedDurably { .. }
                | TargetPublication::PublishedDurabilityUncertain { .. } => {
                    assert_eq!(result.expect("published outcome").publication, publication)
                }
            }
            assert_eq!(*calls.lock().expect("preparation calls"), 1);
        }
    }

    #[test]
    fn same_id_race_reinspects_and_confirms_matching_facts() {
        let source = SessionId::new();
        let request = request(source, 68_803);
        let calls = Arc::new(Mutex::new(0));
        let mut preparation = Preparation {
            source,
            prepared: Some(prepared(source)),
            calls: Arc::clone(&calls),
        };
        let targets = Targets::new(
            [
                Ok(CompactionTargetInspection::Missing),
                Ok(CompactionTargetInspection::Compaction(facts(source))),
            ],
            [Ok(TargetPublication::PublishedDurably { diagnostic: None })],
        );
        targets
            .publications
            .lock()
            .expect("target publications")
            .push_back(TargetPublication::NotPublished {
                diagnostic: "race".to_owned(),
            });
        let _ = futures::executor::block_on(CompactionTransitionService::new(FixedClock).compact(
            request,
            &mut preparation,
            &targets,
        ));
        assert_eq!(*calls.lock().expect("preparation calls"), 1);
        assert_eq!(
            targets.confirmations.lock().expect("confirmations").len(),
            1
        );
    }

    #[test]
    fn same_id_race_with_mismatching_facts_is_a_collision_without_confirmation() {
        let source = SessionId::new();
        let request = request(source, 68_804);
        let calls = Arc::new(Mutex::new(0));
        let mut preparation = Preparation {
            source,
            prepared: Some(prepared(source)),
            calls: Arc::clone(&calls),
        };
        let targets = Targets::new(
            [
                Ok(CompactionTargetInspection::Missing),
                Ok(CompactionTargetInspection::Compaction(facts(
                    SessionId::new(),
                ))),
            ],
            [],
        );
        targets
            .publications
            .lock()
            .expect("target publications")
            .push_back(TargetPublication::NotPublished {
                diagnostic: "race".to_owned(),
            });
        let result =
            futures::executor::block_on(CompactionTransitionService::new(FixedClock).compact(
                request,
                &mut preparation,
                &targets,
            ));
        assert!(matches!(
            result,
            Err(CompactionTransitionError::IdentityCollision { .. })
        ));
        assert_eq!(
            targets.confirmations.lock().expect("confirmations").len(),
            0
        );
    }

    #[test]
    fn matching_retry_confirms_once_without_preparing_or_publishing() {
        let source = SessionId::new();
        let request = request(source, 68_805);
        let calls = Arc::new(Mutex::new(0));
        let mut preparation = Preparation {
            source,
            prepared: None,
            calls: Arc::clone(&calls),
        };
        let targets = Targets::new(
            [Ok(CompactionTargetInspection::Compaction(facts(source)))],
            [Ok(TargetPublication::PublishedDurably { diagnostic: None })],
        );
        let result =
            futures::executor::block_on(CompactionTransitionService::new(FixedClock).compact(
                request,
                &mut preparation,
                &targets,
            ))
            .expect("matching target is confirmed");
        assert_eq!(
            result.publication,
            TargetPublication::PublishedDurably { diagnostic: None }
        );
        assert_eq!(*calls.lock().expect("preparation calls"), 0);
        assert!(
            targets
                .published
                .lock()
                .expect("published seeds")
                .is_empty()
        );
        assert_eq!(
            targets.confirmations.lock().expect("confirmations").len(),
            1
        );
    }
}
