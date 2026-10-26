//! Reading a sandbox refusal out of a finished child.
//!
//! Neither Seatbelt nor Landlock hands the parent a machine-readable denial:
//! the confined program sees `EPERM` and prints it. The only evidence is the
//! program's own `<path>: Operation not permitted` line. That line is written
//! by a process the model chose, so it is never trusted by itself. A path
//! becomes a question for the human only after canonical resolution and floor
//! checks. The command need not name a program-internal target in argv.
//! Stderr does not prove an attempted operation: the suggested human capability
//! below is not residual-write provenance and must never authorize a sentinel.
//! Broad roots, workspace paths and protected floors cannot become questions.

use std::path::{Path, PathBuf};

use ego_core::permit::{
    CanonPath, RunConfinement, SandboxAccess, SandboxGrant, SpawnSpec, StreamProvenance,
    ToolOutputProvenance, ToolResult,
};

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

/// The first absolute path a child reported as EPERM or EACCES.
pub(super) fn analyze(
    spec: &SpawnSpec,
    result: &ToolResult,
    confinement: &RunConfinement,
    granted: &[SandboxGrant],
) -> Option<Denial> {
    if confinement.level() == ego_core::permit::Confinement::Off {
        return None;
    }
    let stderr = stderr(result);
    let mut first = None;
    for line in stderr.lines() {
        let Some(printed) = printed_path(line) else {
            continue;
        };
        // Mode bits alone do not distinguish reading a script from launching
        // it. Shell and launcher diagnostics identify execution refusals.
        let launch_refusal = line.contains(": bad interpreter:")
            || line.starts_with("sandbox-exec: execvp() of '")
            || line.split_once(": ").is_some_and(|(program, _)| {
                matches!(
                    Path::new(program)
                        .file_name()
                        .and_then(|name| name.to_str()),
                    Some("sh" | "bash" | "zsh" | "dash" | "fish")
                )
            })
            || exec_target(spec).is_some_and(|target| target == Path::new(printed));
        if launch_refusal && executable(Path::new(printed)) {
            return Some(Denial {
                printed: printed.to_owned(),
                request: None,
            });
        }
        if floor_refusal(printed, confinement) {
            first.get_or_insert(printed);
            continue;
        }
        let request = request_for(printed, spec, confinement, granted);
        if request.is_some() {
            return Some(Denial {
                printed: printed.to_owned(),
                request,
            });
        }
        first.get_or_insert(printed);
    }
    #[cfg(target_os = "macos")]
    if matches!(
        confinement.level(),
        ego_core::permit::Confinement::Workspace | ego_core::permit::Confinement::Adaptive
    ) && stderr.contains("ERR_PNPM")
        && (stderr.contains("Operation not permitted") || stderr.contains("Permission denied"))
        && let Some(home) = confinement
            .adaptive_perimeter()
            .map(|perimeter| perimeter.home.as_path().to_path_buf())
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        && let Some(path) = super::pnpm::custom_store(&home, confinement.workspace().as_path())
        && let Some(printed) = path.to_str()
        && !floor_refusal(printed, confinement)
        && let Some(request) = request_for(printed, spec, confinement, granted)
    {
        return Some(Denial {
            printed: printed.to_owned(),
            request: Some(request),
        });
    }
    first.map(|printed| Denial {
        printed: printed.to_owned(),
        request: None,
    })
}

/// Everything the child wrote to stderr, in order.
pub(super) fn stderr(result: &ToolResult) -> String {
    result
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
        .join("\n")
}

fn floor_refusal(printed: &str, policy: &RunConfinement) -> bool {
    super::adaptive::floor::blocks(Path::new(printed), policy)
        || canonical(Path::new(printed))
            .is_some_and(|(path, _)| super::adaptive::floor::blocks(path.as_path(), policy))
}

/// `cat: /x/y: Operation not permitted` and `bash: /x/y: Operation not
/// permitted` both name the path between the last separator and the errno.
fn printed_path(line: &str) -> Option<&str> {
    let line = line.trim_end();
    if let Some((_, file)) = line.split_once(": can't open file '") {
        return [
            "': [Errno 1] Operation not permitted",
            "': [Errno 13] Permission denied",
        ]
        .into_iter()
        .find_map(|suffix| file.strip_suffix(suffix))
        .filter(|path| path.starts_with('/'));
    }
    let body = [
        ": Operation not permitted",
        ": Operation not permitted (os error 1)",
        ": Permission denied",
        ": Permission denied (os error 13)",
    ]
    .into_iter()
    .find_map(|suffix| line.strip_suffix(suffix))?;
    if let Some(chain) = body.strip_suffix(": bad interpreter") {
        // Bash prints shell: script: interpreter, or script: interpreter.
        // Prefer the script over the system shell so read-root recovery names
        // the authority that actually prevents launching this command.
        let paths = chain
            .split(": ")
            .filter(|path| path.starts_with('/'))
            .collect::<Vec<_>>();
        return paths
            .iter()
            .copied()
            .find(|path| !path.starts_with("/bin/") && !path.starts_with("/usr/bin/"))
            .or_else(|| paths.last().copied());
    }
    if let Some(program) = refused_program(body) {
        return Some(program);
    }
    let path = body.rsplit_once(": ").map_or(body, |(_, path)| path);
    path.starts_with('/').then_some(path)
}

/// `sandbox-exec: execvp() of '/x/prog' failed` — what `sandbox-exec` prints
/// when the profile does not open the program it was asked to run.
fn refused_program(body: &str) -> Option<&str> {
    let program = body
        .strip_prefix("sandbox-exec: execvp() of '")?
        .strip_suffix("' failed")?;
    program.starts_with('/').then_some(program)
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    }
}

fn request_for(
    printed: &str,
    spec: &SpawnSpec,
    confinement: &RunConfinement,
    granted: &[SandboxGrant],
) -> Option<SandboxGrant> {
    let workspace = confinement.workspace().as_path();
    let (path, exists) = canonical(Path::new(printed))?;
    if path.is_or_under(&CanonPath::new(workspace)) || too_broad(&path, confinement) {
        return None;
    }
    if !renderable(path.as_path()) {
        return None;
    }
    let access = if exec_target(spec).is_some_and(|target| target == path.as_path()) {
        SandboxAccess::Exec
    } else if !exists
        || confinement.adaptive_perimeter().is_some()
        || granted
            .iter()
            .any(|grant| grant.covers(&path, SandboxAccess::Read))
    {
        // Suggest the capability that could unblock this hint to the human:
        // Adaptive already reads every path outside its floors, so a Read
        // grant cannot. Nonexistence/read reach does not prove a write; no
        // judge consumes it.
        SandboxAccess::Write
    } else {
        SandboxAccess::Read
    };
    // A read allowance must never escalate into permission to overwrite an
    // outside-workspace executable, even when stderr suggests a write.
    if access == SandboxAccess::Write && executable(path.as_path()) {
        return None;
    }
    if granted.iter().any(|grant| grant.covers(&path, access)) {
        // Asked and allowed already: asking again cannot change the outcome.
        return None;
    }
    Some(SandboxGrant::new(path, access))
}

/// A grant of `/`, of the home directory, or of a directory above the
/// workspace would hand the command far more than the one path it was refused.
pub(super) fn too_broad(path: &CanonPath, confinement: &RunConfinement) -> bool {
    let above = |other: &Path| CanonPath::new(other).is_or_under(path);
    path.as_path() == Path::new("/")
        || above(confinement.workspace().as_path())
        || match confinement.adaptive_perimeter() {
            Some(perimeter) => above(perimeter.home.as_path()),
            None => std::env::var_os("HOME")
                .and_then(|home| std::fs::canonicalize(home).ok())
                .is_none_or(|home| above(&home)),
        }
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
/// not there yet resolves through its nearest existing ancestor, so the grant
/// still names the exact path a write would create.
pub(super) fn canonical(path: &Path) -> Option<(CanonPath, bool)> {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Some((CanonPath::new(resolved), true));
    }
    let resolved = super::adaptive::resolve_filesystem(path).ok()?;
    Some((CanonPath::new(resolved), false))
}

fn exec_target(spec: &SpawnSpec) -> Option<PathBuf> {
    match spec.identity() {
        Some(identity) => Some(identity.target.as_path().to_path_buf()),
        None => std::fs::canonicalize(spec.argv().first()?).ok(),
    }
}

#[path = "denial_command.rs"]
mod command;
pub(super) use command::command_line;

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
    /// The sandbox judge opened the path for writing for the session. The
    /// command is not run again: it already exited, and nothing proves that
    /// running it twice is safe.
    JudgeGranted,
}

/// The note appended to a refused command's output, for the model and the
/// person reading the tool call.
pub(super) fn note(denial: &Denial, confinement: &RunConfinement, outcome: Outcome) -> String {
    let level = confinement.level();
    if denial.request.is_none() && executable(Path::new(&denial.printed)) {
        let path = canonical(Path::new(&denial.printed)).map(|(path, _)| path);
        let read_root = path.as_ref().and_then(|path| {
            confinement.roots().iter().find(|root| {
                root.access() == ego_core::permit::RootAccess::Read && path.is_or_under(root.path())
            })
        });
        let restriction = read_root.map_or_else(String::new, |root| format!(
            " The read-only root {} forbids execution; exec_paths cannot override it. Change or remove that read-only root in the operator configuration before retrying.", root.path()));
        return format!(
            "sandbox exec denied {} (confinement {level}); no read or write grant can authorize execution.{restriction} Add the program and required interpreter locations to exec_paths in the user configuration with `ego config edit`. Then /quit and run `ego resume <session-id> --mode <same-mode> --sandbox {level} --model <same-model>`; retain the session's --mode, --sandbox and --model selections. No command was retried.",
            denial.printed
        );
    }
    match (&denial.request, outcome) {
        (None, _) if floor_refusal(&denial.printed, confinement) => format!(
            "{} was blocked by the sandbox; the Adaptive floor cannot be lifted by a sandbox grant; \
             credential and Git control protections remain binding",
            denial.printed
        ),
        (Some(request), Outcome::Declined) => format!(
            "sandbox denied {request} (confinement {level}); the user declined to allow it, \
             so the command did not complete; to allow it, run the command again and answer allow"
        ),
        (Some(request), Outcome::NobodyToAsk) => format!(
            "sandbox denied {request} (confinement {level}); no one was available to allow it, \
             so the command did not complete; to allow it, use a session that can ask or a \
             looser --sandbox level"
        ),
        (Some(request), Outcome::CannotWiden) => format!(
            "sandbox denied {request} (confinement {level}); this platform's sandbox cannot open \
             extra paths after the run started, so the command did not complete; to allow it, \
             use a looser --sandbox level"
        ),
        (Some(request), Outcome::JudgeGranted) => format!(
            "sandbox denied {request} (confinement {level}); the sandbox judge allowed writing \
             this path for the rest of the session, but the command did not run again, so it did \
             not complete; run it again if its result is still needed; programs placed there \
             cannot be run"
        ),
        (Some(request), Outcome::QuestionLimit) => format!(
            "sandbox denied {request} (confinement {level}); this command already raised the \
             most sandbox questions one command may ask, so it did not complete; to allow it, \
             run the command again"
        ),
        (None, _) => format!(
            "{} was blocked by the sandbox (confinement {level}), not by a permission rule, so \
             allowing the call does not lift it; exec can only reach the workspace, fixed system \
             paths and paths a person allowed, and a refusal after an allowance may come from \
             macOS privacy protection instead; to allow it, run in the workspace or use a \
             looser --sandbox level",
            denial.printed
        ),
    }
}

#[cfg(test)]
#[path = "denial_tests.rs"]
mod tests;
