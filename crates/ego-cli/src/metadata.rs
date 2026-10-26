//! Durable session metadata execution.

use std::sync::Arc;

use ego_core::event::id::{GoalId, Seq, SessionId};
use ego_core::goal::{GoalOwner, GoalRecord, GoalState, GoalTransition};
use ego_core::run::RunTermination;
use ego_core::session_lifecycle::{
    LeasedLifecycleAppend, LifecycleError, LifecyclePublication, SessionLifecycleStore,
    SessionMetadata,
};
use ego_core::store::evidence::EvidenceRef;
use ego_core::store::session_store::SessionMeta;
use ego_core::turn::SharedJournal;
use ego_tools::clock::SystemClock;
use serde::Serialize;

use crate::cli::{
    GoalOperation, NameOperation, Output, SessionMetadataArgs, SessionMetadataOperation, UsageError,
};
use crate::output::{self, Stdout};
use crate::{catalogue, config, run, session};

pub fn execute(args: &SessionMetadataArgs, out: &Stdout) -> RunTermination {
    match execute_inner(args, out) {
        Ok(()) => RunTermination::Done,
        Err(termination) => termination,
    }
}

fn execute_inner(args: &SessionMetadataArgs, out: &Stdout) -> Result<(), RunTermination> {
    let usage = |error: UsageError| RunTermination::UsageError {
        message: error.to_string(),
    };
    let root = run::workspace(args.directory.as_ref()).map_err(usage)?;
    let store_root = run::rooted_path(
        &root,
        session::store_root(args.session_dir.as_ref()).map_err(usage)?,
    );
    let store = Arc::new(session::jsonl(store_root, root.clone()));
    catalogue::resolve(store.as_ref(), &root, Some(args.session)).map_err(usage)?;
    let metadata = SessionMetadata::new(Arc::clone(&store), Arc::new(SystemClock));

    let opened = match session::open(store.as_ref(), &args.session, &SystemClock) {
        Ok(opened) => opened,
        Err(error) => {
            render_current(&metadata, args, out)?;
            return Err(RunTermination::Failed {
                message: error.to_string(),
            });
        }
    };
    let expected = opened.state.last_seq;

    match &args.operation {
        SessionMetadataOperation::Name(NameOperation::Show) => {
            render_name(args, &metadata.meta(args.session).map_err(failed)?, out);
        }
        SessionMetadataOperation::Name(operation) => {
            let title = match operation {
                NameOperation::Set(title) => Some(title.clone()),
                NameOperation::Clear => None,
                NameOperation::Show => unreachable!(),
            };
            match metadata.set_title(&opened.writer, expected, title) {
                Ok(changed) => {
                    warn_if_publication_issue(changed.publication.as_ref());
                    render_name(args, &changed.value, out);
                }
                Err(error) => return mutation_refusal(&metadata, args, out, error),
            }
        }
        SessionMetadataOperation::Goal(GoalOperation::Show) => {
            let goal = metadata.goal(args.session).map_err(failed)?;
            render_goal(args, goal.as_ref(), out);
        }
        SessionMetadataOperation::Goal(operation) => {
            let changed = match operation {
                GoalOperation::Set(objective) => {
                    let ceilings = config::metadata_ceilings(&root, args).map_err(usage)?;
                    metadata.set_goal(
                        &opened.writer,
                        expected,
                        objective.clone(),
                        GoalOwner::Operator,
                        ceilings,
                    )
                }
                GoalOperation::Achieve => metadata.transition_goal(
                    &opened.writer,
                    expected,
                    GoalTransition::Achieved {
                        attested_by: GoalOwner::Operator,
                        evidence: None,
                    },
                ),
                GoalOperation::Block(reason) => metadata.transition_goal(
                    &opened.writer,
                    expected,
                    GoalTransition::BlockerObserved {
                        reason: reason.clone(),
                    },
                ),
                GoalOperation::Show => unreachable!(),
            };
            match changed {
                Ok(changed) => {
                    warn_if_publication_issue(changed.publication.as_ref());
                    render_goal(args, Some(&changed.value), out);
                }
                Err(error) => return mutation_refusal(&metadata, args, out, error),
            }
        }
    }
    Ok(())
}

pub(crate) fn execute_interactive<S>(
    metadata: &SessionMetadata<S>,
    append: &dyn LeasedLifecycleAppend,
    journal: &SharedJournal<'_>,
    operation: &SessionMetadataOperation,
    ceilings: &ego_core::ceiling::Ceilings,
    emit: &mut impl FnMut(&str),
) -> Result<(), LifecycleError>
where
    S: SessionLifecycleStore,
{
    let current = journal
        .inspect(Clone::clone)
        .map_err(|error| LifecycleError::Store(error.into()))?;
    let expected = current.last_seq;
    let result = match operation {
        SessionMetadataOperation::Name(NameOperation::Show) => {
            emit(&name_text_from_title(current.title.as_deref()));
            return Ok(());
        }
        SessionMetadataOperation::Name(operation) => {
            let title = match operation {
                NameOperation::Set(title) => Some(title.clone()),
                NameOperation::Clear => None,
                NameOperation::Show => unreachable!(),
            };
            metadata.set_title(append, expected, title).map(|changed| {
                emit_publication_warning(changed.publication.as_ref(), emit);
                emit(&name_text(&changed.value));
            })
        }
        SessionMetadataOperation::Goal(GoalOperation::Show) => {
            emit(&goal_text(current.goal.as_ref()));
            return Ok(());
        }
        SessionMetadataOperation::Goal(operation) => {
            let changed = match operation {
                GoalOperation::Set(objective) => metadata.set_goal(
                    append,
                    expected,
                    objective.clone(),
                    GoalOwner::Operator,
                    ceilings.clone(),
                ),
                GoalOperation::Achieve => metadata.transition_goal(
                    append,
                    expected,
                    GoalTransition::Achieved {
                        attested_by: GoalOwner::Operator,
                        evidence: None,
                    },
                ),
                GoalOperation::Block(reason) => metadata.transition_goal(
                    append,
                    expected,
                    GoalTransition::BlockerObserved {
                        reason: reason.clone(),
                    },
                ),
                GoalOperation::Show => unreachable!(),
            };
            changed.map(|changed| {
                emit_publication_warning(changed.publication.as_ref(), emit);
                emit(&goal_text(Some(&changed.value)));
            })
        }
    };

    if let Err(error) = result {
        if let Ok(state) = journal.inspect(Clone::clone) {
            match operation {
                SessionMetadataOperation::Name(_) => {
                    emit(&name_text_from_title(state.title.as_deref()))
                }
                SessionMetadataOperation::Goal(_) => emit(&goal_text(state.goal.as_ref())),
            }
        }
        return Err(error);
    }
    Ok(())
}

fn mutation_refusal<S>(
    metadata: &SessionMetadata<S>,
    args: &SessionMetadataArgs,
    out: &Stdout,
    error: LifecycleError,
) -> Result<(), RunTermination>
where
    S: ego_core::session_lifecycle::SessionLifecycleStore,
{
    if matches!(&error, LifecycleError::InvalidText) {
        return Err(RunTermination::UsageError {
            message: error.to_string(),
        });
    }
    render_current(metadata, args, out)?;
    Err(failed(error))
}

fn render_current<S>(
    metadata: &SessionMetadata<S>,
    args: &SessionMetadataArgs,
    out: &Stdout,
) -> Result<(), RunTermination>
where
    S: ego_core::session_lifecycle::SessionLifecycleStore,
{
    match &args.operation {
        SessionMetadataOperation::Name(_) => {
            render_name(args, &metadata.meta(args.session).map_err(failed)?, out)
        }
        SessionMetadataOperation::Goal(_) => {
            let goal = metadata.goal(args.session).map_err(failed)?;
            render_goal(args, goal.as_ref(), out);
        }
    }
    Ok(())
}

fn failed(error: impl std::fmt::Display) -> RunTermination {
    RunTermination::Failed {
        message: error.to_string(),
    }
}

fn warn_if_publication_issue(publication: Option<&LifecyclePublication>) {
    if let Some(warning) = publication_warning(publication) {
        output::note(&warning);
    }
}

fn emit_publication_warning(
    publication: Option<&LifecyclePublication>,
    emit: &mut impl FnMut(&str),
) {
    if let Some(warning) = publication_warning(publication) {
        emit(&warning);
    }
}

fn publication_warning(publication: Option<&LifecyclePublication>) -> Option<String> {
    match publication {
        Some(LifecyclePublication::PublishedWithProjectionWarning { diagnostic }) => Some(format!(
            "session details changed, but the saved session list could not be updated: {diagnostic}"
        )),
        Some(LifecyclePublication::PublishedDurabilityUncertain { diagnostic }) => Some(format!(
            "session details changed, but saving them to disk could not be confirmed: {diagnostic}"
        )),
        Some(LifecyclePublication::NotPublished { .. })
        | Some(LifecyclePublication::Published)
        | None => None,
    }
}

#[derive(Serialize)]
struct NameReport<'a> {
    v: u8,
    session_id: SessionId,
    name: Option<&'a str>,
}

#[derive(Serialize)]
struct GoalReport<'a> {
    v: u8,
    session_id: SessionId,
    goal: Option<GoalView<'a>>,
}

#[derive(Serialize)]
struct GoalView<'a> {
    id: GoalId,
    objective: &'a str,
    owner: GoalOwner,
    ceilings: &'a ego_core::ceiling::Ceilings,
    derived_from: Option<GoalId>,
    created_seq: Seq,
    state: GoalState,
    blocker_reason: Option<&'a str>,
    blocker_observations: u64,
    achieved_by: Option<GoalOwner>,
    achievement_evidence: Option<&'a EvidenceRef>,
}

impl<'a> From<&'a GoalRecord> for GoalView<'a> {
    fn from(goal: &'a GoalRecord) -> Self {
        Self {
            id: goal.id,
            objective: &goal.objective,
            owner: goal.owner,
            ceilings: &goal.ceilings,
            derived_from: goal.derived_from,
            created_seq: goal.created_seq,
            state: goal.state,
            blocker_reason: goal.blocker_reason.as_deref(),
            blocker_observations: goal.blocker_observations,
            achieved_by: goal.achieved_by,
            achievement_evidence: goal.achievement_evidence.as_ref(),
        }
    }
}

fn render_name(args: &SessionMetadataArgs, meta: &SessionMeta, out: &Stdout) {
    match args.output {
        Output::Text => out.line(&name_text(meta)),
        Output::Json | Output::Jsonl => render_machine(
            &NameReport {
                v: 1,
                session_id: args.session,
                name: meta.title.as_deref(),
            },
            out,
        ),
    }
}

fn name_text(meta: &SessionMeta) -> String {
    name_text_from_title(meta.title.as_deref())
}

fn name_text_from_title(title: Option<&str>) -> String {
    format!("name: {}", title.unwrap_or("(none)"))
}

fn render_goal(args: &SessionMetadataArgs, goal: Option<&GoalRecord>, out: &Stdout) {
    match args.output {
        Output::Text => out.line(&goal_text(goal)),
        Output::Json | Output::Jsonl => render_machine(
            &GoalReport {
                v: 1,
                session_id: args.session,
                goal: goal.map(GoalView::from),
            },
            out,
        ),
    }
}

fn goal_text(goal: Option<&GoalRecord>) -> String {
    let Some(goal) = goal else {
        return "goal: (none)".to_owned();
    };
    match goal.state {
        GoalState::Active => match (&goal.blocker_reason, goal.blocker_observations) {
            (Some(reason), observations) if observations != 0 => format!(
                "goal active: {} (blocked {observations}/{}: {reason})",
                goal.objective,
                ego_core::goal::BLOCKER_OBSERVATION_LIMIT
            ),
            _ => format!("goal active: {}", goal.objective),
        },
        GoalState::Achieved => format!("goal achieved: {}", goal.objective),
        GoalState::Blocked => format!(
            "goal blocked: {} ({})",
            goal.objective,
            goal.blocker_reason.as_deref().unwrap_or("unknown")
        ),
    }
}

fn render_machine(value: &impl Serialize, out: &Stdout) {
    match serde_json::to_string(value) {
        Ok(line) => out.line(&line),
        Err(error) => output::note(&format!("session metadata could not be rendered: {error}")),
    }
}

#[cfg(test)]
mod regression_case_tests {
    use super::*;
    use ego_core::event::domain::EventKind;
    use ego_core::event::id::{Producer, Seq, TurnId};
    use ego_core::event::time::{Clock, Timestamp};
    use ego_core::session_lifecycle::{LeasedSharedJournal, LifecycleError, SessionLifecycleStore};
    use ego_core::sink::NullSink;
    use ego_tools::store::{JsonlStore, OsIo};
    use std::path::Path;

    struct FixedClock;
    impl Clock for FixedClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_millis(1)
        }
    }

    /// Catches: metadata reports /3 although the durable goal blocks on observation five.
    #[test]
    fn goal_blocker_receipts_follow_the_five_observation_lifecycle() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(JsonlStore::new(
            root.path(),
            workspace.path(),
            Box::new(OsIo),
        ));
        let id = SessionId::new();
        store
            .publish_empty_session(id, workspace.path(), Timestamp::from_millis(1))
            .unwrap();
        let opened = crate::session::open(store.as_ref(), &id, &FixedClock).unwrap();
        let metadata = SessionMetadata::new(Arc::clone(&store), Arc::new(FixedClock));
        let journal = LeasedSharedJournal::new(
            &opened.writer,
            &FixedClock,
            &NullSink,
            Producer::Main,
            opened.state.clone(),
        );
        let shared = journal.shared();
        let ceilings = ego_core::ceiling::Ceilings::default();
        let mut notices = Vec::new();
        execute_interactive(
            &metadata,
            &journal,
            &shared,
            &SessionMetadataOperation::Goal(GoalOperation::Set("ship".into())),
            &ceilings,
            &mut |notice| notices.push(notice.to_owned()),
        )
        .unwrap();
        for observation in 1..=5 {
            execute_interactive(
                &metadata,
                &journal,
                &shared,
                &SessionMetadataOperation::Goal(GoalOperation::Block("network".into())),
                &ceilings,
                &mut |notice| notices.push(notice.to_owned()),
            )
            .unwrap();
            let expected = if observation == 5 {
                "goal blocked: ship (network)".to_owned()
            } else {
                format!("goal active: ship (blocked {observation}/5: network)")
            };
            assert_eq!(notices.last().unwrap(), &expected);
        }
    }

    #[test]
    fn regression_case_runtime_epoch_persists_and_replays() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(JsonlStore::new(
            root.path(),
            workspace.path(),
            Box::new(OsIo),
        ));
        let id = ego_core::event::id::SessionId::new();
        store
            .publish_empty_session(id, Path::new(workspace.path()), Timestamp::from_millis(1))
            .unwrap();
        let opened = crate::session::open(store.as_ref(), &id, &FixedClock).unwrap();
        let metadata = SessionMetadata::new(Arc::clone(&store), Arc::new(FixedClock));
        let journal = LeasedSharedJournal::new(
            &opened.writer,
            &FixedClock,
            &NullSink,
            Producer::Main,
            opened.state.clone(),
        );
        let shared = journal.shared();
        let mut notices = Vec::new();
        let ceilings = ego_core::ceiling::Ceilings::default();
        execute_interactive(
            &metadata,
            &journal,
            &shared,
            &SessionMetadataOperation::Name(NameOperation::Set("project alpha".into())),
            &ceilings,
            &mut |n| notices.push(n.to_owned()),
        )
        .unwrap();
        assert_eq!(notices.last().unwrap(), "name: project alpha");
        notices.clear();
        let before = shared.inspect(|state| state.last_seq).unwrap();
        assert!(matches!(
            execute_interactive(
                &metadata,
                &journal,
                &shared,
                &SessionMetadataOperation::Name(NameOperation::Set(" \t".into())),
                &ceilings,
                &mut |n| notices.push(n.to_owned())
            ),
            Err(LifecycleError::InvalidText)
        ));
        assert_eq!(notices, ["name: project alpha"]);
        assert_eq!(shared.inspect(|state| state.last_seq).unwrap(), before);
        execute_interactive(
            &metadata,
            &journal,
            &shared,
            &SessionMetadataOperation::Goal(GoalOperation::Set("ship cleanly".into())),
            &ceilings,
            &mut |n| notices.push(n.to_owned()),
        )
        .unwrap();
        assert_eq!(notices.last().unwrap(), "goal active: ship cleanly");
        shared
            .append(
                TurnId::new(),
                None,
                None,
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: "ordinary".into(),
                    images: Vec::new(),
                    queued_prompt: None,
                },
            )
            .unwrap();
        let expected = {
            let mut seq = Seq::ZERO;
            seq.next();
            seq.next();
            seq.next()
        };
        drop(shared);
        let state = journal.try_into_state().unwrap();
        assert_eq!(state.last_seq, expected);
        assert_eq!(state.title.as_deref(), Some("project alpha"));
        assert_eq!(
            state.goal.as_ref().map(|goal| goal.objective.as_str()),
            Some("ship cleanly")
        );
        assert_eq!(
            state.goal.as_ref().map(|goal| goal.state),
            Some(ego_core::goal::GoalState::Active)
        );
        drop(opened);
        let reopened = crate::session::open(store.as_ref(), &id, &FixedClock).unwrap();
        assert_eq!(reopened.state.last_seq, expected);
        assert_eq!(reopened.state.title.as_deref(), Some("project alpha"));
        assert_eq!(
            reopened
                .state
                .goal
                .as_ref()
                .map(|goal| goal.objective.as_str()),
            Some("ship cleanly")
        );
        assert_eq!(
            reopened.state.goal.as_ref().map(|goal| goal.state),
            Some(ego_core::goal::GoalState::Active)
        );
    }
}
