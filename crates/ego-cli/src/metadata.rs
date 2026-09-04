//! Non-interactive durable session metadata execution.

use std::sync::Arc;

use ego_core::event::id::{GoalId, Seq, SessionId};
use ego_core::goal::{GoalOwner, GoalRecord, GoalState, GoalTransition};
use ego_core::run::RunTermination;
use ego_core::session_lifecycle::{LifecycleError, LifecyclePublication, SessionMetadata};
use ego_core::store::evidence::EvidenceRef;
use ego_core::store::session_store::SessionMeta;
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
    match publication {
        Some(LifecyclePublication::PublishedWithProjectionWarning { diagnostic }) => {
            output::note(&format!(
                "metadata is visible, but its manifest projection failed: {diagnostic}"
            ));
        }
        Some(LifecyclePublication::PublishedDurabilityUncertain { diagnostic }) => {
            output::note(&format!(
                "metadata is visible, but its durability is uncertain: {diagnostic}"
            ));
        }
        Some(LifecyclePublication::NotPublished { .. })
        | Some(LifecyclePublication::Published)
        | None => {}
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
        Output::Text => out.line(&format!(
            "name: {}",
            meta.title.as_deref().unwrap_or("(none)")
        )),
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
                "goal active: {} (blocked {observations}/3: {reason})",
                goal.objective
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
