use std::path::Path;

use ego_core::permit::{CanonPath, RunConfinement, SpawnSpec};

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

/// One shell word, quoted only when a shell would read it as more than one.
fn quote(argument: &str) -> String {
    let plain = !argument.is_empty()
        && argument
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        return argument.to_owned();
    }
    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('\'');
    for c in argument.chars() {
        match c {
            '\'' => quoted.push_str("'\\''"),
            c if c.is_control() || is_bidi_control(c) => {
                quoted.push_str(&format!("\\u{{{:x}}}", u32::from(c)));
            }
            c => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}

/// Embeddings, overrides and isolates (U+202A-202E, U+2066-2069) and the
/// directional marks (U+200E, U+200F, U+061C).
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{61c}')
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
    if directory.is_or_under(workspace) || too_broad(&directory, workspace.as_path()) {
        return None;
    }
    Some(format!(
        "; to allow writes there in later sessions, run `ego roots add --access read-write {}`, \
         then exit and resume the session",
        quote(&directory.as_path().to_string_lossy())
    ))
}
