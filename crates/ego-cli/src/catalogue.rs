//! The session catalogue: which sessions belong here, and how they are shown.
//!
//! Two commands read it — `ego ls` prints it and `ego resume` selects one row
//! from it — and they agree about *which* rows because they share
//! [`in_workspace`] and [`by_recency`] rather than each filtering and sorting
//! its own way. A list whose first row is not the row `ego resume` would pick
//! is a list that lies about what the next command does.
//!
//! `docs/decisions/2026-08-21-the-two-commands-the-header-unblocked.md`.

use std::path::Path;

use ego_core::event::id::SessionId;
use ego_core::event::time::Timestamp;
use ego_core::session::SessionLineage;
use ego_core::store::session_store::{SessionMeta, SessionStore, StoreError};
use serde::Serialize;

use crate::cli::UsageError;

/// The version of the shape `--output json` writes (`CLI.md` §4.6).
///
/// Its own number, not `PUBLIC_EVENT_VERSION`. That one versions the event
/// stream a run publishes; this versions a catalogue listing, and the two
/// change for unrelated reasons — sharing a number would make a new event
/// field bump a contract that did not move.
pub const CATALOGUE_VERSION: u32 = 3;

/// The sessions belonging to `root`, most recently updated first.
pub fn in_workspace(metas: Vec<SessionMeta>, root: &Path) -> Vec<SessionMeta> {
    let mut kept: Vec<SessionMeta> = metas
        .into_iter()
        .filter(|meta| meta.workspace_root == root)
        .collect();
    by_recency(&mut kept);
    kept
}

/// Most recently updated first, and the id breaks a tie.
///
/// Two sessions can share an `updated` — the timestamp is milliseconds and a
/// scripted pair of runs can land inside one. Leaving that to the order
/// `list()` happened to read directories in would make `ego ls` and
/// `ego resume` disagree about "latest" on exactly the runs a script produces.
pub fn by_recency(metas: &mut [SessionMeta]) {
    metas.sort_by(|a, b| {
        b.updated
            .cmp(&a.updated)
            .then_with(|| a.id.to_string().cmp(&b.id.to_string()))
    });
}

/// The session `ego resume` continues.
///
/// `id` is what the caller typed, already known to parse. `None` means the
/// latest of this workspace.
pub fn resolve(
    store: &dyn SessionStore,
    root: &Path,
    id: Option<SessionId>,
) -> Result<SessionId, UsageError> {
    let metas = store
        .list()
        .map_err(|err| UsageError(format!("the session store could not be listed: {err}")))?;

    let Some(wanted) = id else {
        return in_workspace(metas, root)
            .first()
            .map(|meta| meta.id)
            .ok_or_else(|| {
                UsageError(format!(
                    "no session belongs to {} yet: run `ego run` here first, or \
                     name a session id",
                    root.display()
                ))
            });
    };

    // Named explicitly, so the catalogue is asked whether it exists rather
    // than asked to choose. A `run` would have created it; `resume` must not
    // (ADR §3), so an id nobody has used is a refusal and not a fresh session.
    //
    // `{wanted}` and not `{wanted:?}` in both refusals below: these reach an
    // operator, and `SessionId(0192…)` names a Rust type they cannot act on.
    // The bare id is the thing they can pass back to `ego resume`.
    let Some(meta) = metas.into_iter().find(|meta| meta.id == wanted) else {
        return Err(UsageError(format!(
            "no session {wanted} in this store: `ego ls --all` lists the ones \
             there are"
        )));
    };

    // Its history was recorded against one tree and this run's tools would
    // work in another, so continuing it here is not continuing it. Both paths
    // are named, which is all `-C` needs to fix it.
    if meta.workspace_root != root {
        return Err(UsageError(format!(
            "session {wanted} belongs to {}, and this is {}: resuming it here \
             would run its history against a different tree. Use `-C {}`",
            meta.workspace_root.display(),
            root.display(),
            meta.workspace_root.display()
        )));
    }
    Ok(meta.id)
}

/// One row of `--output json` and one line of `--output jsonl` (`CLI.md` §4.6).
///
/// A shape of its own rather than `SessionMeta` serialised directly: that type
/// is the store port's, and publishing it would make a field added for a
/// store's benefit a contract a script depends on.
#[derive(Debug, Serialize)]
pub struct Row {
    pub id: SessionId,
    pub workspace_root: String,
    pub model: Option<String>,
    pub created: u64,
    pub updated: u64,
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineage: Option<SessionLineage>,
}

impl Row {
    pub fn of(meta: &SessionMeta) -> Self {
        Row {
            id: meta.id,
            workspace_root: meta.workspace_root.display().to_string(),
            model: meta.model.clone(),
            created: meta.created.as_millis(),
            updated: meta.updated.as_millis(),
            title: meta.title.clone(),
            lineage: meta.lineage,
        }
    }
}

/// The single object `ego ls --output json` writes (`CLI.md` §4.6).
///
/// An object with a `sessions` key and not a bare array, so the listing can
/// carry its own `v` — a top-level array has nowhere to put one, and a
/// consumer that cannot see the version is a consumer that cannot be told the
/// shape changed.
#[derive(Debug, Serialize)]
pub struct Listing {
    pub v: u32,
    pub sessions: Vec<Row>,
}

impl Listing {
    pub fn of(metas: &[SessionMeta]) -> Self {
        Listing {
            v: CATALOGUE_VERSION,
            sessions: metas.iter().map(Row::of).collect(),
        }
    }
}

/// The table `ego ls` prints to a terminal.
///
/// Columns are padded to the widest cell rather than to a fixed width, because
/// a model slug is `ollama/qwen3` for one operator and
/// `openrouter/z-ai/glm-4.7` for another, and a truncated slug is not one an
/// operator can pass back to `--model`.
///
/// `workspace_root` is a column only under `--all`. Without it every row would
/// carry the same path — the workspace the caller is standing in — which is
/// the definition of a column that says nothing.
pub fn table(metas: &[SessionMeta], all: bool) -> Vec<String> {
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(metas.len() + 1);
    let mut header = vec![
        "ID".to_string(),
        "UPDATED".to_string(),
        "MODEL".to_string(),
        "TITLE".to_string(),
    ];
    if all {
        header.push("WORKSPACE".to_string());
    }
    rows.push(header);

    for meta in metas {
        // An em dash and not an empty cell: a column that is blank for every
        // row reads as a rendering fault, and "nothing has ever set this"
        // is a fact worth showing rather than a gap worth hiding.
        let dash = || "—".to_string();
        let mut row = vec![
            meta.id.to_string(),
            stamp(meta.updated.as_millis()),
            meta.model.clone().unwrap_or_else(dash),
            meta.title.clone().unwrap_or_else(dash),
        ];
        if all {
            row.push(meta.workspace_root.display().to_string());
        }
        rows.push(row);
    }

    plain_table(rows)
}

/// Pad a plain-text table to its widest cells without trailing whitespace.
pub(crate) fn plain_table(rows: Vec<Vec<String>>) -> Vec<String> {
    let columns = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..columns)
        .map(|at| {
            rows.iter()
                .map(|row| row[at].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();

    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                // The last column is not padded: trailing spaces on every line
                // are invisible until something diffs them.
                .map(|(at, cell)| match at + 1 == columns {
                    true => cell.clone(),
                    false => format!("{cell:<width$}", width = widths[at]),
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        })
        .collect()
}

/// `updated` as a UTC calendar time an operator can read.
///
/// The calendar arithmetic itself lives on [`Timestamp`], because this is not
/// the only surface that needs a date out of one — the ACP `SessionInfo` wire
/// wants the same instant spelled RFC 3339 — and two renderings of one clock
/// must not be two implementations of one calendar. What stays here is only
/// the shape `ego ls` prints: minute precision, no seconds, no `T`.
fn stamp(millis: u64) -> String {
    if millis == 0 {
        return "—".to_string();
    }
    let at = Timestamp::from_millis(millis).civil();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        at.year, at.month, at.day, at.hour, at.minute
    )
}

/// What `ego ls` was asked for, listed and ordered.
pub fn listing(
    store: &dyn SessionStore,
    root: &Path,
    all: bool,
) -> Result<Vec<SessionMeta>, StoreError> {
    let metas = store.list()?;
    Ok(if all {
        let mut every = metas;
        by_recency(&mut every);
        every
    } else {
        in_workspace(metas, root)
    })
}

/// Interactive rows use current durable prose rather than a stale manifest cache.
pub(crate) fn saved_sessions(
    store: &ego_tools::store::JsonlStore,
) -> Result<Vec<SessionMeta>, StoreError> {
    use ego_core::session_lifecycle::SessionLifecycleStore;
    let mut metas = Vec::new();
    for listed in store.list()? {
        if let Some(meta) = store.lifecycle_meta(listed.id)? {
            metas.push(meta);
        }
    }
    by_recency(&mut metas);
    Ok(metas)
}
