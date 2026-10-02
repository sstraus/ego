//! Reading a sandbox refusal out of a finished child.
//!
//! Neither Seatbelt nor Landlock hands the parent a machine-readable denial:
//! the confined program sees `EPERM` and prints it. The only evidence is the
//! program's own `<path>: Operation not permitted` line. That line is written
//! by a process the model chose, so it is never trusted by itself. A path
//! becomes a question for the human only when the command line the human
//! already approved also names it, it resolves to a location the profile does
//! not already open, and it is not inside the workspace the run owns.

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
    let printed = stderr.lines().find_map(printed_path)?;
    let request = request_for(
        printed,
        &argv,
        spec,
        confinement.workspace().as_path(),
        granted,
    );
    Some(Denial {
        printed: printed.to_owned(),
        request,
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
    // The command line the human approved must name the path. Without this a
    // program could print any path it likes and have the human asked about it.
    if !argv.iter().any(|argument| argument.contains(printed)) {
        return None;
    }
    let (path, exists) = canonical(Path::new(printed))?;
    if path.is_or_under(&CanonPath::new(workspace)) {
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

/// How a refused command ended, for the note appended to its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Declined,
    NobodyToAsk,
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
    use super::printed_path;

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
}
