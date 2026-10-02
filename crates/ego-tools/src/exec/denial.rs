//! Reading a sandbox refusal out of a finished child.
//!
//! Neither Seatbelt nor Landlock hands the parent a machine-readable denial:
//! the confined program sees `EPERM` and prints it. The only evidence is the
//! program's own `<path>: Operation not permitted` line. That line is written
//! by a process the model chose, so it is never trusted by itself. A path
//! becomes a question for the human only when the command line the human
//! already approved names it (or a directory above it), it resolves to a
//! location the profile does not already open and can express, it is not
//! inside the workspace the run owns, and it is not a place so broad that
//! opening it would open the person's whole machine (`/`, the home directory,
//! or any directory above the workspace).

use std::path::{Path, PathBuf};

use ego_core::permit::{
    CanonPath, RunConfinement, SandboxAccess, SandboxGrant, SpawnSpec, StreamProvenance,
    ToolOutputProvenance, ToolResult,
};

const EPERM_SUFFIX: &str = ": Operation not permitted";

/// What a failed confined command said about the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Denial {
    /// The path exactly as the child printed it.
    pub(super) printed: String,
    /// The grant that would let the command reach it, or `None` when the
    /// refusal cannot be answered by a human (already granted, inside the
    /// workspace, or not resolvable).
    pub(super) request: Option<SandboxGrant>,
}

/// The first absolute path a child reported as `Operation not permitted`.
pub(super) fn analyze(
    spec: &SpawnSpec,
    result: &ToolResult,
    confinement: &RunConfinement,
    granted: &[SandboxGrant],
) -> Option<Denial> {
    if confinement.level() == ego_core::permit::Confinement::Off {
        return None;
    }
    let argv = spec
        .argv()
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let stderr = result
        .content
        .iter()
        .filter(|block| {
            matches!(
                block.provenance,
                ToolOutputProvenance::Process {
                    stream: StreamProvenance::Stderr,
                    ..
                }
            )
        })
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut first = None;
    for printed in stderr.lines().filter_map(printed_path) {
        let request = request_for(
            printed,
            &argv,
            spec,
            confinement.workspace().as_path(),
            granted,
        );
        if request.is_some() {
            return Some(Denial {
                printed: printed.to_owned(),
                request,
            });
        }
        first.get_or_insert(printed);
    }
    first.map(|printed| Denial {
        printed: printed.to_owned(),
        request: None,
    })
}

/// `cat: /x/y: Operation not permitted` and `bash: /x/y: Operation not
/// permitted` both name the path between the last separator and the errno.
fn printed_path(line: &str) -> Option<&str> {
    let body = line.trim_end().strip_suffix(EPERM_SUFFIX)?;
    let path = body.rsplit_once(": ").map_or(body, |(_, path)| path);
    path.starts_with('/').then_some(path)
}

fn request_for(
    printed: &str,
    argv: &[String],
    spec: &SpawnSpec,
    workspace: &Path,
    granted: &[SandboxGrant],
) -> Option<SandboxGrant> {
    let (path, exists) = canonical(Path::new(printed))?;
    // The command line the human approved must name the path, or a directory
    // above it: without this a program could print any path it likes and have
    // the human asked about it. Names are compared by resolved path component,
    // never as substrings (`/a/Gits` is not named by `/a/Gits-evil/f`).
    if !argv_anchors(argv, workspace).any(|anchor| path.is_or_under(&anchor)) {
        return None;
    }
    if path.is_or_under(&CanonPath::new(workspace)) || too_broad(&path, workspace) {
        return None;
    }
    if !renderable(path.as_path()) {
        return None;
    }
    let access = if exec_target(spec).is_some_and(|target| target == path.as_path()) {
        SandboxAccess::Exec
    } else if !exists
        || granted
            .iter()
            .any(|grant| grant.covers(&path, SandboxAccess::Read))
    {
        // A path that does not exist cannot have been refused for reading,
        // and a path that is already readable can only be refused for writing.
        SandboxAccess::Write
    } else {
        SandboxAccess::Read
    };
    if granted.iter().any(|grant| grant.covers(&path, access)) {
        // Asked and allowed already: asking again cannot change the outcome.
        return None;
    }
    Some(SandboxGrant::new(path, access))
}

/// The absolute paths the command line spells, resolved: whole arguments and
/// the value of `--flag=/path`. A place too broad to grant anchors nothing
/// either: a command line that merely names `/`, or a workspace symlink to the
/// home directory, does not name what lies below it.
fn argv_anchors<'a>(
    argv: &'a [String],
    workspace: &'a Path,
) -> impl Iterator<Item = CanonPath> + 'a {
    argv.iter().filter_map(|argument| {
        let spelled = match argument.split_once('=') {
            Some((_, value)) if value.starts_with('/') => value,
            _ => argument.as_str(),
        };
        spelled
            .starts_with('/')
            .then(|| canonical(Path::new(spelled)))
            .flatten()
            .map(|(path, _)| path)
            .filter(|path| !too_broad(path, workspace))
    })
}

/// A grant of `/`, of the home directory, or of a directory above the
/// workspace would hand the command far more than the one path it was refused.
fn too_broad(path: &CanonPath, workspace: &Path) -> bool {
    let above = |other: &Path| CanonPath::new(other).is_or_under(path);
    path.as_path() == Path::new("/")
        || above(workspace)
        || std::env::var_os("HOME")
            .and_then(|home| std::fs::canonicalize(home).ok())
            .is_some_and(|home| above(&home))
}

#[cfg(target_os = "macos")]
pub(super) fn renderable(path: &Path) -> bool {
    super::profile::renderable(path)
}

/// Landlock takes the path as a handle, so every path is expressible.
#[cfg(not(target_os = "macos"))]
pub(super) fn renderable(_: &Path) -> bool {
    true
}

/// The resolved location, and whether the path itself exists. A path that is
/// not there yet resolves through its parent, so the grant names the exact
/// file a write would create.
fn canonical(path: &Path) -> Option<(CanonPath, bool)> {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Some((CanonPath::new(resolved), true));
    }
    let parent = std::fs::canonicalize(path.parent()?).ok()?;
    Some((CanonPath::new(parent.join(path.file_name()?)), false))
}

fn exec_target(spec: &SpawnSpec) -> Option<PathBuf> {
    match spec.identity() {
        Some(identity) => Some(identity.target.as_path().to_path_buf()),
        None => std::fs::canonicalize(spec.argv().first()?).ok(),
    }
}

/// The command line as the person reads it, whole: the person approves the
/// re-run on this text, so no part of it is cut. An argument with any
/// character a shell would read as more than a word is single-quoted, and a
/// control or bidirectional-control character is spelled `\u{..}` so a host
/// terminal can neither act on it nor reorder the text around it.
pub(super) fn command_line(spec: &SpawnSpec) -> String {
    spec.argv()
        .iter()
        .map(|argument| {
            let argument = argument.to_string_lossy();
            let plain = !argument.is_empty()
                && argument
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
            if plain {
                return argument.into_owned();
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
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Embeddings, overrides and isolates (U+202A-202E, U+2066-2069) and the
/// directional marks (U+200E, U+200F, U+061C).
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{61c}')
}

/// How a refused command ended, for the note appended to its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Declined,
    NobodyToAsk,
    /// The person answered every question one command may raise, and the
    /// command was refused again.
    QuestionLimit,
    /// This platform's sandbox cannot open a path after the run was admitted.
    CannotWiden,
}

/// The note appended to a refused command's output, for the model and the
/// person reading the tool call.
pub(super) fn note(denial: &Denial, confinement: &RunConfinement, outcome: Outcome) -> String {
    let level = confinement.level();
    match (&denial.request, outcome) {
        (Some(request), Outcome::Declined) => format!(
            "sandbox denied {request} (confinement {level}); the user declined to allow it, \
             so the command did not complete"
        ),
        (Some(request), Outcome::NobodyToAsk) => format!(
            "sandbox denied {request} (confinement {level}); no one was available to allow it, \
             so the command did not complete"
        ),
        (Some(request), Outcome::CannotWiden) => format!(
            "sandbox denied {request} (confinement {level}); this platform's sandbox cannot open \
             extra paths after the run started, so the command did not complete"
        ),
        (Some(request), Outcome::QuestionLimit) => format!(
            "sandbox denied {request} (confinement {level}); this command already raised the \
             most sandbox questions one command may ask, so it did not complete"
        ),
        (None, _) => format!(
            "{} was refused with 'Operation not permitted' under confinement {level}; exec can \
             only reach the workspace, fixed system paths and paths a person allowed, and a \
             refusal after an allowance may come from macOS privacy protection instead",
            denial.printed
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{command_line, printed_path};
    use ego_core::permit::{EnvPolicy, SpawnSpec};

    // Lines recorded from macOS `cat`, `ls` and `touch` under `sandbox-exec`
    // with a profile that does not open the path, 2026-10-02.
    #[test]
    fn recorded_macos_tool_lines_yield_their_path() {
        assert_eq!(
            printed_path("cat: /Users/u/Gits/.tmp/x//outside.txt: Operation not permitted"),
            Some("/Users/u/Gits/.tmp/x//outside.txt")
        );
        assert_eq!(
            printed_path("touch: /Users/u/new: Operation not permitted"),
            Some("/Users/u/new")
        );
        assert_eq!(
            printed_path("/bin/sh: /Users/u/o2: Operation not permitted"),
            Some("/Users/u/o2")
        );
    }

    #[test]
    fn other_errors_and_relative_paths_are_not_sandbox_questions() {
        assert_eq!(
            printed_path("cat: /Users/u/missing: No such file or directory"),
            None
        );
        assert_eq!(
            printed_path("cat: notes.txt: Operation not permitted"),
            None
        );
        assert_eq!(printed_path("Operation not permitted"), None);
    }

    #[test]
    fn the_command_line_quotes_what_a_person_could_misread() {
        let spec = SpawnSpec::new(
            ["/bin/sh", "-c", "echo hi there", ""]
                .map(Into::into)
                .to_vec(),
            None,
            None,
            EnvPolicy::Inherited,
        )
        .expect("spawn spec");
        assert_eq!(command_line(&spec), "/bin/sh -c 'echo hi there' ''");
    }
}
