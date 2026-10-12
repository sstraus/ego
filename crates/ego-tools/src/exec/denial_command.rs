use std::path::Path;

use ego_core::permit::{CanonPath, RunConfinement, SpawnSpec, shell_word as quote};

use super::{canonical, too_broad};

/// The command line as the person reads it, whole: the person approves the
/// re-run on this text, so no part of it is cut. An argument with any
/// character a shell would read as more than a word is single-quoted, and a
/// control or bidirectional-control character is spelled `\u{..}` so a host
/// terminal can neither act on it nor reorder the text around it.
pub(in crate::exec) fn command_line(spec: &SpawnSpec) -> String {
    spec.argv()
        .iter()
        .map(|argument| quote(&argument.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Adaptive reads are open outside its floors, so any other refusal is a
/// write. Name the operator command that opens the containing directory for
/// later sessions, unless that directory is the workspace's or too broad.
pub(super) fn roots_command(printed: &str, confinement: &RunConfinement) -> Option<String> {
    if confinement.level() != ego_core::permit::Confinement::Adaptive {
        return None;
    }
    let (path, _) = canonical(Path::new(printed))?;
    let directory = CanonPath::new(path.as_path().parent()?);
    let workspace = confinement.workspace();
    if directory.is_or_under(workspace) || too_broad(&directory, confinement) {
        return None;
    }
    Some(format!(
        "; to allow writes there in later sessions, run `ego roots add --access read-write {}`, \
         then exit and resume the session",
        quote(&directory.as_path().to_string_lossy())
    ))
}
