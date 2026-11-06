//! `ego rewind ID --at ITEM_ID`: restore the session's files after a turn,
//! then fork the conversation there. The ordering policy is
//! [`crate::rewind`]; this is the command around it.

use std::sync::Arc;

use ego_core::run::RunTermination;
use ego_core::session::ReplaySource as _;
use ego_core::session_lifecycle::SessionLifecycle;
use ego_core::state::SessionState;
use ego_tools::clock::SystemClock;

use super::session_commands::{NoLiveRuns, render, render_fork_result};
use super::{Ended, report, rooted_path, workspace};
use crate::cli::{Output, RewindArgs, UsageError};
use crate::output::{Diagnostics as _, Stdout};
use crate::session;

/// `ego rewind ID --at ITEM_ID`.
pub async fn rewind(args: &RewindArgs, out: &Stdout) -> Ended {
    match rewound(args, out).await {
        Ok(ended) => ended,
        Err(termination) => Ended::refused(termination),
    }
}

async fn rewound(args: &RewindArgs, out: &Stdout) -> Result<Ended, RunTermination> {
    use ego_core::session::SessionLineage;
    use ego_core::session_lifecycle::SessionLifecycleStore as _;
    use ego_tools::checkpoint::CheckpointFs;

    let usage = |error: UsageError| RunTermination::UsageError {
        message: error.to_string(),
    };
    let failed = |message: String| RunTermination::Failed { message };
    let root = workspace(args.directory.as_ref()).map_err(usage)?;
    let store_root = rooted_path(
        &root,
        session::store_root(args.session_dir.as_ref()).map_err(usage)?,
    );
    let store = Arc::new(session::jsonl(store_root, root.clone()));
    let source =
        crate::catalogue::resolve(store.as_ref(), &root, Some(args.source)).map_err(usage)?;
    // No other process may write the session, or its files through it,
    // while they are rewound.
    let _lease = store.acquire_inactive_lease(source).map_err(|error| {
        failed(format!(
            "session {source} is in use, so it cannot be rewound: {error}"
        ))
    })?;
    let lifecycle = SessionLifecycle::new(
        Arc::clone(&store),
        Arc::new(NoLiveRuns),
        Arc::new(SystemClock),
    );
    let prepared = lifecycle
        .prepare_fork_at(source, args.at)
        .map_err(|error| failed(format!("the rewind failed: {error}")))?;
    let SessionLineage::Fork {
        source: owner,
        at_seq,
    } = prepared.seed().lineage()
    else {
        return Err(failed("the rewind failed: no fork boundary".into()));
    };
    if owner != source {
        return Err(failed(format!(
            "message {} belongs to the earlier session {owner}, which this session was forked from; rewind {owner} instead",
            args.at
        )));
    }
    let entries = store
        .entries(&source)
        .map_err(|error| failed(format!("cannot read session {source}: {error}")))?
        .unwrap_or_default();
    let recorded = crate::rewind::recorded(&entries, at_seq);
    if !recorded.unsaved.is_empty() {
        return Err(failed(format!(
            "nothing was rewound: {}",
            crate::rewind::Refusal::Unsaved(recorded.unsaved)
        )));
    }
    let (before, after) = (recorded.before, recorded.after);
    let chain = CheckpointFs::history(&root, source)
        .map_err(|error| failed(format!("cannot read the checkpoints of {source}: {error}")))?;
    let order = crate::rewind::plan(&chain, &before, &after)
        .map_err(|refusal| failed(format!("nothing was rewound: {refusal}")))?;

    // Report every file changed outside ego before anything is restored.
    let mut changed = Vec::new();
    for checkpoint in crate::rewind::newest_per_file(&order, &chain) {
        let preview = CheckpointFs::preview_one(&root, source, checkpoint)
            .map_err(|error| failed(format!("cannot check checkpoint {checkpoint}: {error}")))?;
        if preview.conflict {
            changed.push(format!("  {}", preview.path.display()));
        }
    }
    let mut images = std::collections::HashMap::new();
    for checkpoint in &order {
        let image = CheckpointFs::images(&root, source, checkpoint)
            .map_err(|error| failed(format!("cannot check checkpoint {checkpoint}: {error}")))?;
        images.insert(checkpoint.clone(), image);
    }
    for path in crate::rewind::changed_between(&order, &chain, &images) {
        let line = format!("  {}", path.display());
        if !changed.contains(&line) {
            changed.push(line);
        }
    }
    if !changed.is_empty() {
        return Err(failed(format!(
            "nothing was rewound: these files changed outside ego after that turn; keep or undo those changes first:\n{}",
            changed.join("\n")
        )));
    }

    let mut restored = Vec::new();
    for checkpoint in &order {
        let preview = CheckpointFs::restore(&root, source, checkpoint).map_err(|error| {
            failed(format!(
                "restoring checkpoint {checkpoint} failed: {error}. {} earlier restore(s) were applied and no session was created; fix the cause and run the same rewind again (restores repeat safely)",
                restored.len()
            ))
        })?;
        restored.push(preview);
    }
    let created = lifecycle
        .fork_at(source, args.at)
        .await
        .map_err(|error| {
            failed(format!(
                "the files were restored, but the fork failed: {error}. Run the same rewind again to create the session"
            ))
        })?;
    for preview in &restored {
        if !preview.already_original {
            out.note(&format!(
                "restored {} ({})",
                preview.path.display(),
                preview.effect
            ));
        }
    }
    if args.output == Output::Text {
        return Ok(render_fork_result(
            created.id,
            &created.publication,
            Output::Text,
            out,
        ));
    }
    let mut ended = Ended {
        termination: RunTermination::Done,
        session: created.id,
        state: SessionState::default(),
        result: serde_json::json!({
            "session": created.id.to_string(),
            "source": source.to_string(),
            "at": args.at.to_string(),
            "restored": restored.iter().map(|preview| serde_json::json!({
                "checkpoint": preview.checkpoint,
                "path": preview.path,
                "effect": preview.effect,
                "already_original": preview.already_original,
            })).collect::<Vec<_>>(),
        }),
        selected_session: None,
        mcp_errors: Vec::new(),
    };
    if let Err(termination) = render(out, &report(&ended), "the rewind result") {
        ended.termination = termination;
    }
    Ok(ended)
}
