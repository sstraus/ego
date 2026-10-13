use ego_core::permit::{SpawnSpec, shell_word as quote};

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
