//! `ego rewind ID --at ITEM_ID`: restore the files a session changed after a
//! completed turn, then fork the conversation after that turn (`CLI.md` §6.7).
//!
//! The source session is not changed: its journal stays as it was, the file
//! restores are recorded as restore checkpoints in its checkpoint ref, and
//! the new session's lineage names the source and the turn. That is the
//! audit trail.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use ego_core::event::domain::EventKind;
use ego_core::event::id::Seq;
use ego_core::store::session_store::SessionEntry;
use ego_tools::checkpoint::HistoryEntry;

/// Why the files cannot be rewound as asked. Nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The session's checkpoint chain has writes after the turn that no
    /// journaled tool result names: a `task` child's writes, or writes from
    /// a version that did not record checkpoint ids.
    Unrecorded(Vec<(String, PathBuf)>),
    /// A recorded checkpoint is not in this repository's chain: it was
    /// closed, or it belongs to another repository.
    Missing(Vec<String>),
    /// Builtin writes after the turn saved no checkpoint (checkpoints are
    /// off, or the file is outside Git), so nothing can restore them.
    Unsaved(Vec<String>),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unrecorded(entries) => {
                writeln!(
                    f,
                    "these files were changed after that turn by writes that the session does not record (a task child, or an older ego), so rewind cannot restore them in order:"
                )?;
                for (checkpoint, path) in entries {
                    writeln!(f, "  {} (checkpoint {checkpoint})", path.display())?;
                }
                write!(
                    f,
                    "Restore them with `ego checkpoint ID restore`, or fork with `ego fork --at`."
                )
            }
            Self::Unsaved(paths) => {
                writeln!(
                    f,
                    "these files were changed after that turn by writes that saved no checkpoint (checkpoints are off, or the file is outside Git), so rewind cannot restore them:"
                )?;
                for path in paths {
                    writeln!(f, "  {path}")?;
                }
                write!(
                    f,
                    "Turn checkpoints on (`ego config set checkpoints=true`) for later edits, or fork with `ego fork --at`."
                )
            }
            Self::Missing(checkpoints) => write!(
                f,
                "the session recorded checkpoints that this repository does not have ({}); run rewind with -C in the repository the session wrote to, or note that `ego checkpoint close` removed them",
                checkpoints.join(", ")
            ),
        }
    }
}

/// What the journal records about file writes, split at the turn.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recorded {
    /// Checkpoints created at or before the turn, in journal order.
    pub before: Vec<String>,
    /// Checkpoints created after the turn, in journal order.
    pub after: Vec<String>,
    /// Paths that a successful builtin write changed after the turn without
    /// a checkpoint.
    pub unsaved: Vec<String>,
}

/// The builtin tools that write files through the checkpointing filesystem.
const WRITERS: [&str; 3] = ["write", "edit", "notebook_edit"];

/// Read the session's file writes, split at `boundary`.
pub fn recorded(entries: &[SessionEntry], boundary: Seq) -> Recorded {
    let mut recorded = Recorded::default();
    // A completed call names its tool only through the started event of
    // the same item.
    let mut writes: HashMap<_, String> = HashMap::new();
    for entry in entries {
        let SessionEntry::Event(event) = entry else {
            continue;
        };
        match &event.kind {
            EventKind::ToolCallStarted { name, args, .. } if WRITERS.contains(&name.as_str()) => {
                if let Some(item) = event.env.item {
                    let path = serde_json::from_str::<serde_json::Value>(args)
                        .ok()
                        .and_then(|args| args.get("path")?.as_str().map(str::to_owned))
                        .unwrap_or_else(|| format!("({name} call)"));
                    writes.insert(item, path);
                }
            }
            EventKind::ToolCallCompleted { ok, result } => {
                let after = event.env.seq > boundary;
                if !result.file_writes().is_empty() {
                    for file in result.file_writes() {
                        match file.checkpoint.as_deref() {
                            Some(checkpoint) if after => recorded.after.push(checkpoint.to_owned()),
                            Some(checkpoint) => recorded.before.push(checkpoint.to_owned()),
                            None if after
                                && file.possibly_modified
                                && !recorded.unsaved.contains(&file.path) =>
                            {
                                recorded.unsaved.push(file.path.clone())
                            }
                            None => {}
                        }
                    }
                    continue;
                }
                if !ok {
                    continue;
                }
                match result.checkpoint() {
                    Some(checkpoint) if after => recorded.after.push(checkpoint.to_owned()),
                    Some(checkpoint) => recorded.before.push(checkpoint.to_owned()),
                    None if after => {
                        if let Some(path) = event.env.item.and_then(|item| writes.get(&item))
                            && !recorded.unsaved.contains(path)
                        {
                            recorded.unsaved.push(path.clone());
                        }
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }
    recorded
}

/// The checkpoints to restore, newest first, or why not.
///
/// `chain` is the session's checkpoint chain, newest first. Every write
/// checkpoint newer than the last one recorded at or before the turn must be
/// one recorded after it, so a write the journal does not name is never left
/// out silently.
pub fn plan(
    chain: &[HistoryEntry],
    before: &[String],
    after: &[String],
) -> Result<Vec<String>, Refusal> {
    let known: HashSet<&str> = chain
        .iter()
        .map(|entry| entry.checkpoint.as_str())
        .collect();
    let missing: Vec<String> = after
        .iter()
        .filter(|checkpoint| !known.contains(checkpoint.as_str()))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(Refusal::Missing(missing));
    }
    let anchor = before
        .iter()
        .rev()
        .find(|checkpoint| known.contains(checkpoint.as_str()));
    let recorded_after: HashSet<&str> = after.iter().map(String::as_str).collect();
    let mut order = Vec::new();
    let mut unrecorded = Vec::new();
    for entry in chain {
        if anchor.is_some_and(|anchor| *anchor == entry.checkpoint) {
            break;
        }
        if entry.restoration {
            continue;
        }
        if recorded_after.contains(entry.checkpoint.as_str()) {
            order.push(entry.checkpoint.clone());
        } else {
            unrecorded.push((entry.checkpoint.clone(), entry.path.clone()));
        }
    }
    if !unrecorded.is_empty() {
        return Err(Refusal::Unrecorded(unrecorded));
    }
    Ok(order)
}

/// For each file, the newest checkpoint to restore: the one whose
/// post-image must still be on disk, or the file changed outside ego.
pub fn newest_per_file<'a>(order: &'a [String], chain: &[HistoryEntry]) -> Vec<&'a str> {
    let paths: HashMap<&str, &PathBuf> = chain
        .iter()
        .map(|entry| (entry.checkpoint.as_str(), &entry.path))
        .collect();
    let mut seen = HashSet::new();
    order
        .iter()
        .filter(|checkpoint| {
            paths
                .get(checkpoint.as_str())
                .is_some_and(|path| seen.insert(*path))
        })
        .map(String::as_str)
        .collect()
}

/// The files changed outside ego between two of the writes to restore.
///
/// `images` maps a checkpoint to its (pre-image, post-image) blob ids.
/// Walking `order` newest first, an older write of a file must have left the
/// bytes that the next newer write of that file found; otherwise something
/// else changed the file in between, and restoring past it would stop half
/// way at the older checkpoint's conflict.
pub fn changed_between(
    order: &[String],
    chain: &[HistoryEntry],
    images: &HashMap<String, (String, String)>,
) -> Vec<PathBuf> {
    let paths: HashMap<&str, &PathBuf> = chain
        .iter()
        .map(|entry| (entry.checkpoint.as_str(), &entry.path))
        .collect();
    let mut found: HashMap<&PathBuf, &str> = HashMap::new();
    let mut changed = Vec::new();
    for checkpoint in order {
        let (Some(path), Some((before, after))) =
            (paths.get(checkpoint.as_str()), images.get(checkpoint))
        else {
            continue;
        };
        if let Some(newer_found) = found.get(path)
            && newer_found != after
            && !changed.contains(*path)
        {
            changed.push((*path).clone());
        }
        found.insert(path, before);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(checkpoint: &str, path: &str, restoration: bool) -> HistoryEntry {
        HistoryEntry {
            checkpoint: checkpoint.into(),
            path: path.into(),
            restoration,
        }
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    // Catches: restoring the writes after the turn oldest first, so the older
    // restore sees the newer post-image and refuses, or the file ends at an
    // intermediate state; and walking past the turn into earlier writes.
    #[test]
    fn writes_after_the_turn_restore_newest_first_and_stop_at_the_turn() {
        let chain = [
            entry("c4", "a.txt", false),
            entry("c3", "b.txt", false),
            entry("r1", "a.txt", true),
            entry("c2", "a.txt", false),
            entry("c1", "a.txt", false),
        ];
        let order = plan(&chain, &ids(&["c1"]), &ids(&["c2", "c3", "c4"])).unwrap();
        assert_eq!(order, ids(&["c4", "c3", "c2"]));
        assert_eq!(newest_per_file(&order, &chain), vec!["c4", "c3"]);
    }

    // Catches: a task child's write after the turn (in the chain, named by no
    // journaled result) left out silently, so the rewind reports success
    // with that file still changed.
    #[test]
    fn a_write_no_result_names_refuses_the_rewind() {
        let chain = [
            entry("c3", "child.txt", false),
            entry("c2", "a.txt", false),
            entry("c1", "a.txt", false),
        ];
        assert_eq!(
            plan(&chain, &ids(&["c1"]), &ids(&["c2"])),
            Err(Refusal::Unrecorded(vec![("c3".into(), "child.txt".into())]))
        );
    }

    // Catches: an edit outside ego between two later writes of one file
    // passing the check of the newest write only, so the newest restore is
    // applied and the older one then refuses, leaving the file half rewound.
    #[test]
    fn a_file_changed_between_two_later_writes_is_found_before_any_restore() {
        let chain = [
            entry("c3", "a.txt", false),
            entry("c2", "a.txt", false),
            entry("c4", "b.txt", false),
        ];
        let images = |pairs: &[(&str, &str, &str)]| {
            pairs
                .iter()
                .map(|(id, before, after)| {
                    (
                        (*id).to_owned(),
                        ((*before).to_owned(), (*after).to_owned()),
                    )
                })
                .collect::<HashMap<_, _>>()
        };
        let order = ids(&["c3", "c2", "c4"]);
        let consistent = images(&[
            ("c3", "three", "four"),
            ("c2", "two", "three"),
            ("c4", "x", "y"),
        ]);
        assert!(changed_between(&order, &chain, &consistent).is_empty());
        let edited = images(&[
            ("c3", "formatted", "four"),
            ("c2", "two", "three"),
            ("c4", "x", "y"),
        ]);
        assert_eq!(
            changed_between(&order, &chain, &edited),
            vec![PathBuf::from("a.txt")]
        );
    }

    // Catches: a session whose checkpoints were closed rewinding only its
    // conversation while reporting the files as restored.
    #[test]
    fn a_recorded_checkpoint_absent_from_the_chain_refuses_the_rewind() {
        let chain = [entry("c1", "a.txt", false)];
        assert_eq!(
            plan(&chain, &ids(&["c1"]), &ids(&["gone"])),
            Err(Refusal::Missing(ids(&["gone"])))
        );
    }
}
