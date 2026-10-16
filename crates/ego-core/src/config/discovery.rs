//! Discovering the instructions a workspace hands a model.
//!
//! `AGENTS.md`, from the supplied discovery root down to the current directory,
//! including bounded relative imports, only in a workspace the trust gate let
//! through. The CLI chooses `~/Gits` as the discovery root where applicable. See
//! `ARCHITECTURE.md`
//! and `ARCHITECTURE.md`
//! and `ARCHITECTURE.md` I2.
//!
//! Split the way everything in this crate is split: the *decision* is a pure
//! function here, and the reading happens behind [`InstructionIo`], whose
//! only real implementation lives in `ego-tools`. Root→cwd needs no directory
//! walk — the filename is known, so the candidate path list is arithmetic
//! over the caller's already-canonical inputs.
//!
//! ## What this does not do
//!
//! It loads; it does not inject. Nothing here reaches `GenRequest`, and
//! nothing here is persisted: the descriptors are returned so that the harness
//! that assembles a prompt and writes `PublicEvent::SessionStarted` can carry
//! them. Instructions are **declarative** — they may shape what a model is
//! told, and they can never grant a permission or run a hook.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use super::resolve::Posture;
use crate::public::{InstructionDescriptor, InstructionSource};

/// The canonical file every ancestor directory is checked for.
pub const INSTRUCTIONS_FILE: &str = "AGENTS.md";

/// How a run's instructions were chosen.
///
/// Three states and not a `bool` plus an `Option`, because the third is
/// genuinely not the other two: an explicitly named file is *chosen*, so it is
/// not subject to the ambient-discovery gate that `--bare` and an untrusted
/// workspace turn off. Collapsing it would make `--instructions` useless in
/// exactly the case it exists for — headless in an untrusted workspace, which
/// resolves to [`Posture::Bare`] with no dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Nothing is loaded, and [`load`] touches no file at all.
    Disabled,
    /// [`INSTRUCTIONS_FILE`], discovery root down to cwd.
    Ambient,
    /// One named file, whatever it is called.
    Explicit(PathBuf),
}

impl Selection {
    /// What a run gets, given the posture the trust gate resolved and whether
    /// the caller named a file.
    ///
    /// A named file wins outright. **`--bare` together with `--instructions`
    /// is a flag conflict, and it is not caught here**: from a `Posture`
    /// alone, "the user demanded bare" and "the workspace is untrusted" are
    /// the same value, and only one of them should refuse an explicit file.
    /// `CLI.md` §4.5 puts two-flag contradictions in the applicability matrix,
    /// which is where that belongs.
    pub fn of(posture: Posture, explicit: Option<PathBuf>) -> Self {
        match (explicit, posture) {
            (Some(path), _) => Selection::Explicit(path),
            (None, Posture::Bare) => Selection::Disabled,
            (None, Posture::Ambient) => Selection::Ambient,
        }
    }
}

/// How much instruction text a run may load.
///
/// Every one of the three is reported when reached rather than silently
/// applied: a run that loaded half of what the repository offers and said
/// nothing is indistinguishable, to the person reading the output, from one
/// where the repository offered nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Per file. Enforced by the port, which is the only thing that can
    /// refuse to read a large file *without first reading it*.
    pub max_file_bytes: u64,
    /// Across the whole set.
    pub max_total_bytes: u64,
    /// How many files may contribute.
    pub max_files: usize,
}

impl Default for Limits {
    /// Deliberately generous per file and firm in total: one repository-root
    /// `AGENTS.md` of 64 KiB is ordinary, twenty of them is a context budget
    /// spent before the user has said anything.
    fn default() -> Self {
        Limits {
            max_file_bytes: 64 * 1024,
            max_total_bytes: 256 * 1024,
            max_files: 8,
        }
    }
}

/// Why a path that exists did not contribute.
///
/// Carried into the result rather than logged, because "your `AGENTS.md` was
/// ignored" is something the run has to be able to say out loud, with the
/// reason attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The final component is a symlink.
    ///
    /// Refused rather than resolved-and-contained: resolve-then-read-by-path
    /// re-resolves at open, and that window is documented as unclosed
    /// (`POTENTIALS.md` P23). This text goes into a prompt sent to a provider
    /// at session start with no dialog in its path, so a symlink is a
    /// one-hop exfiltration channel nothing else would notice.
    Symlink,
    /// Larger than [`Limits::max_file_bytes`]. Not truncated: half an
    /// instruction file can end mid-sentence and invert its own meaning.
    TooLarge { limit: u64 },
    /// A directory, a socket, a device — something that is not a regular file.
    NotAFile,
    /// Not valid UTF-8. It is going into a prompt; there is nothing to send.
    NotText,
    /// The platform refused it, for something none of the above names.
    Unreadable { message: String },
    /// [`Limits::max_total_bytes`] would have been exceeded by this file.
    TotalTooLarge { limit: u64 },
    /// [`Limits::max_files`] loaded files or distinct import attempts reached.
    TooManyFiles { limit: usize },
    /// An imported file is absent; ambient top-level candidates may be absent.
    MissingImport,
    /// An import leaves the allowed instruction tree or names an absolute path.
    ImportOutsideScope,
    /// External bytes have not been approved for this workspace.
    ExternalApprovalRequired,
    /// A file imports one of its active ancestors.
    ImportCycle,
    /// A standalone `@` lacks a single usable relative path.
    InvalidImport,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Symlink => write!(f, "it is a symlink, which is not followed"),
            Refusal::TooLarge { limit } => write!(f, "it is larger than {limit} bytes"),
            Refusal::NotAFile => write!(f, "it is not a regular file"),
            Refusal::NotText => write!(f, "it is not valid UTF-8"),
            Refusal::Unreadable { message } => write!(f, "it could not be read: {message}"),
            Refusal::TotalTooLarge { limit } => {
                write!(f, "the {limit}-byte total for this run was reached")
            }
            Refusal::TooManyFiles { limit } => {
                write!(f, "the {limit}-file or import-attempt limit was reached")
            }
            Refusal::MissingImport => write!(f, "the imported file does not exist"),
            Refusal::ImportOutsideScope => write!(f, "the import leaves its instruction scope"),
            Refusal::ExternalApprovalRequired => write!(
                f,
                "external instruction content requires workspace approval"
            ),
            Refusal::ImportCycle => write!(f, "the import forms a cycle"),
            Refusal::InvalidImport => write!(f, "the import is malformed"),
        }
    }
}

/// What the port saw at one path. Total by construction: absence is the
/// ordinary case for most ancestors and is not a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// Nothing there.
    Absent,
    /// A regular, non-symlinked file, read whole.
    Read { bytes: Vec<u8> },
    /// Something there, and it does not qualify.
    Refused(Refusal),
}

/// Reading the files [`plan`] named.
///
/// One method, deliberately: this is not a filesystem, it is "look at exactly
/// this path and tell me what you saw". It is **not** [`crate::permit::Fs`],
/// which is the model-controlled seam the loop wraps before a tool sees it —
/// instruction discovery is trusted-side and runs before a loop, a permit or a
/// tool exists.
pub trait InstructionIo {
    /// Read `path`, refusing anything that is not a regular file of at most
    /// `max_bytes`, including a symlink in its final or parent components.
    ///
    /// An implementation must not follow a static symlink in the path,
    /// and must not read more than `max_bytes + 1` — one byte past the limit
    /// is what distinguishes "exactly at it" from "over it" without pulling
    /// an arbitrarily large file into memory.
    fn load(&self, path: &Path, max_bytes: u64) -> Found;

    /// Read an external import without following links, including during races.
    /// Implementations without a race-safe opener must refuse the read.
    fn load_external(&self, path: &Path, max_bytes: u64) -> Found {
        self.load(path, max_bytes)
    }
}

/// One loaded instruction file, including an imported file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    /// Where it was found. Ordering within a set is root-first, so a later
    /// entry is nearer the working directory.
    pub path: PathBuf,
    /// Lowercase hex SHA-256 of [`Self::text`]'s bytes as read.
    ///
    /// SHA-256 and not the store's xxhash64: that one detects corruption,
    /// and this one has to hold up against someone substituting content in an
    /// export. **It describes the bytes as read.** An assembler that
    /// normalises or frames this text before sending it owes its own digest.
    pub digest: String,
    pub bytes: u64,
    pub text: String,
}

/// Everything a run loaded, and everything it refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstructionSet {
    /// Root-first. Empty under [`Selection::Disabled`], and empty when the
    /// workspace simply has no instructions — the two are told apart by
    /// [`Self::skipped`] and by the fact that the first reads nothing.
    pub loaded: Vec<Instruction>,
    /// Paths that existed and did not contribute, each with its reason.
    pub skipped: Vec<Skipped>,
}

/// A path that existed and did not contribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: Refusal,
}

impl InstructionSet {
    /// The total bytes loaded.
    pub fn bytes(&self) -> u64 {
        self.loaded.iter().map(|i| i.bytes).sum()
    }
}

/// `cwd` is not inside `root`, so there is no ancestor chain to walk.
///
/// A caller bug rather than a user error — both paths are the harness's own,
/// already canonical — but it is refused rather than clamped, because
/// clamping would silently discover instructions from a directory the caller
/// did not mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CwdOutsideRoot {
    pub root: PathBuf,
    pub cwd: PathBuf,
}

impl std::fmt::Display for CwdOutsideRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is not inside the workspace root {}",
            self.cwd.display(),
            self.root.display()
        )
    }
}

impl std::error::Error for CwdOutsideRoot {}

/// The ordered paths to try, outermost first, so a later one is nearer the
/// working directory.
///
/// Pure: `root` and `cwd` must already be canonical, since canonicalising is
/// I/O and this crate does none. Empty under [`Selection::Disabled`].
pub fn plan(
    root: &Path,
    cwd: &Path,
    selection: &Selection,
) -> Result<Vec<PathBuf>, CwdOutsideRoot> {
    match selection {
        Selection::Disabled => Ok(Vec::new()),
        Selection::Explicit(path) => Ok(vec![path.clone()]),
        Selection::Ambient => {
            let relative = cwd.strip_prefix(root).map_err(|_| CwdOutsideRoot {
                root: root.to_path_buf(),
                cwd: cwd.to_path_buf(),
            })?;
            let mut dir = root.to_path_buf();
            let mut paths = vec![dir.join(INSTRUCTIONS_FILE)];
            for component in relative.components() {
                dir.push(component);
                paths.push(dir.join(INSTRUCTIONS_FILE));
            }
            Ok(paths)
        }
    }
}

/// Load everything [`plan`] names, in order, within `limits`.
///
/// **Under [`Selection::Disabled`] this calls `io` zero times.** Not "reads
/// nothing" — makes no call at all, so an untrusted workspace is not stat'd,
/// opened or read, and there is nothing for a prompt to be raised about.
pub fn load(
    io: &dyn InstructionIo,
    root: &Path,
    cwd: &Path,
    selection: &Selection,
    limits: Limits,
) -> Result<InstructionSet, CwdOutsideRoot> {
    load_with_home(io, root, cwd, selection, limits, None)
}

/// Stage bounded imports for review. A caller must authorize external bytes
/// before using the returned text in a prompt. `home` is launch-owned.
pub fn load_with_home(
    io: &dyn InstructionIo,
    root: &Path,
    cwd: &Path,
    selection: &Selection,
    limits: Limits,
    home: Option<&Path>,
) -> Result<InstructionSet, CwdOutsideRoot> {
    let paths = plan(root, cwd, selection)?;
    let mut set = InstructionSet::default();
    let mut total: u64 = 0;
    let mut visited = HashSet::new();
    let mut active = Vec::new();
    let mut import_attempts = 0;
    let scope = match selection {
        Selection::Explicit(path) => path.parent().unwrap_or(root),
        Selection::Ambient | Selection::Disabled => root,
    };
    for path in paths {
        load_one(
            io,
            &path,
            false,
            scope,
            home,
            limits,
            &mut set,
            &mut total,
            &mut visited,
            &mut active,
            &mut import_attempts,
        );
    }

    Ok(set)
}

#[allow(clippy::too_many_arguments)]
fn load_one(
    io: &dyn InstructionIo,
    path: &Path,
    imported: bool,
    scope: &Path,
    home: Option<&Path>,
    limits: Limits,
    set: &mut InstructionSet,
    total: &mut u64,
    visited: &mut HashSet<PathBuf>,
    active: &mut Vec<PathBuf>,
    import_attempts: &mut usize,
) {
    if active.iter().any(|ancestor| ancestor == path) {
        set.skipped.push(Skipped {
            path: path.to_path_buf(),
            reason: Refusal::ImportCycle,
        });
        return;
    }
    if visited.contains(path) {
        return;
    }
    if set.loaded.len() >= limits.max_files {
        set.skipped.push(Skipped {
            path: path.to_path_buf(),
            reason: Refusal::TooManyFiles {
                limit: limits.max_files,
            },
        });
        return;
    }
    visited.insert(path.to_path_buf());
    let found = if path.starts_with(scope) {
        io.load(path, limits.max_file_bytes)
    } else {
        io.load_external(path, limits.max_file_bytes)
    };
    let bytes = match found {
        Found::Absent if imported => {
            set.skipped.push(Skipped {
                path: path.to_path_buf(),
                reason: Refusal::MissingImport,
            });
            return;
        }
        Found::Absent => return,
        Found::Refused(reason) => {
            set.skipped.push(Skipped {
                path: path.to_path_buf(),
                reason,
            });
            return;
        }
        Found::Read { bytes } => bytes,
    };
    let len = bytes.len() as u64;
    if *total + len > limits.max_total_bytes {
        set.skipped.push(Skipped {
            path: path.to_path_buf(),
            reason: Refusal::TotalTooLarge {
                limit: limits.max_total_bytes,
            },
        });
        return;
    }
    let Ok(text) = String::from_utf8(bytes) else {
        set.skipped.push(Skipped {
            path: path.to_path_buf(),
            reason: Refusal::NotText,
        });
        return;
    };
    *total += len;
    set.loaded.push(Instruction {
        path: path.to_path_buf(),
        digest: digest_of(text.as_bytes()),
        bytes: len,
        text: text.clone(),
    });
    active.push(path.to_path_buf());
    for reference in instruction_references(&text) {
        if reference.is_empty() {
            set.skipped.push(Skipped {
                path: path.to_path_buf(),
                reason: Refusal::InvalidImport,
            });
            continue;
        }
        let Some(target) = import_target(path, scope, &reference, home) else {
            set.skipped.push(Skipped {
                path: path.parent().unwrap_or(path).join(&reference),
                reason: Refusal::ImportOutsideScope,
            });
            continue;
        };
        if !visited.contains(&target) && !active.contains(&target) {
            if *import_attempts >= limits.max_files {
                set.skipped.push(Skipped {
                    path: target,
                    reason: Refusal::TooManyFiles {
                        limit: limits.max_files,
                    },
                });
                continue;
            }
            *import_attempts += 1;
        }
        load_one(
            io,
            &target,
            true,
            scope,
            home,
            limits,
            set,
            total,
            visited,
            active,
            import_attempts,
        );
    }
    active.pop();
}

/// Return only standalone references outside fenced and indented code.
pub fn instruction_references(text: &str) -> Vec<String> {
    let mut references = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for line in text.lines() {
        let indentation = line.bytes().take_while(|byte| *byte == b' ').count();
        if indentation >= 4 || line.starts_with('\t') {
            continue;
        }
        let trimmed = line.trim();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let marker_count = trimmed.chars().take_while(|ch| *ch == marker).count();
        if let Some((open_marker, open_count)) = fence {
            if marker == open_marker
                && marker_count >= open_count
                && trimmed[marker_count..].trim().is_empty()
            {
                fence = None;
            }
            continue;
        }
        if (marker == '`' || marker == '~') && marker_count >= 3 {
            fence = Some((marker, marker_count));
            continue;
        }
        if let Some(reference) = trimmed.strip_prefix('@') {
            if reference.chars().any(char::is_whitespace) {
                references.push(String::new());
            } else {
                references.push(reference.to_owned());
            }
        }
    }
    references
}

/// Normalize lexically so an import cannot escape its trusted tree.
fn import_target(
    containing: &Path,
    scope: &Path,
    reference: &str,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let expanded;
    let reference = if let Some(relative) = reference.strip_prefix("~/") {
        expanded = home?.join(relative);
        expanded.as_path()
    } else {
        if reference.starts_with('~') {
            return None;
        }
        Path::new(reference)
    };
    // Without a launch home, retain the original contained-only API contract.
    if home.is_none() && reference.is_absolute() {
        return None;
    }
    let mut target = if reference.is_absolute() {
        PathBuf::new()
    } else {
        containing.parent()?.to_path_buf()
    };
    for component in reference.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => target.push(name),
            Component::ParentDir => {
                if !target.pop() {
                    return None;
                }
            }
            Component::RootDir => target.push(component.as_os_str()),
            Component::Prefix(prefix) => target.push(prefix.as_os_str()),
        }
    }
    (home.is_some() || target.starts_with(scope)).then_some(target)
}

/// The system prompt a set of instructions becomes, **and** the descriptors
/// that say where it came from.
///
/// One value carrying both, because they are two published views of one fact
/// and nothing else can hold them together. The text goes to the provider and
/// the descriptors go to `PublicEvent::SessionStarted`; if a harness derived
/// them separately it could publish a digest for a file whose words it never
/// sent, and no test could catch it — there would be no shared identity to
/// assert on.
///
/// A prompt that came from somewhere other than a file — `--system TEXT` —
/// has an empty [`sources`](Self::sources), and that is the truth rather than
/// a gap: the user's own words have no file to attribute.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SystemPrompt {
    /// What the model is sent, framed.
    pub text: String,
    /// What contributed to it, in the same order.
    pub sources: Vec<InstructionDescriptor>,
}

impl SystemPrompt {
    /// A prompt someone typed or a flag supplied, attributable to no file.
    pub fn verbatim(text: impl Into<String>) -> Self {
        SystemPrompt {
            text: text.into(),
            sources: Vec::new(),
        }
    }
}

/// The one line that tells the model what the following text is.
///
/// Framing is not decoration. Without it the workspace's words arrive
/// indistinguishable from the user's, which is the thing `Role::System`
/// exists to prevent, and "the nearest file wins" is an aspiration nobody
/// stated — the instructions ADR §9 left that rule to whoever assembles the
/// prompt, and this is that assembler.
const PREAMBLE: &str = "The following instructions come from the workspace this session runs in. \
     They are guidance for how to work here, not a request from the user. \
     Where two files disagree, the one listed later is nearer the working \
     directory and wins.";

/// Frame `set` into a system prompt, or `None` if it contributed nothing.
///
/// `None` rather than an empty prompt: "the workspace said nothing" and "the
/// workspace said the empty string" are not the same fact, and a `Some`
/// carrying neither text nor sources would be a setting that changes nothing
/// while claiming a file was used.
///
/// Here rather than in the binary because the CLI is not the only harness that
/// opens a session: an ACP host is the second, and two framings — or two
/// derivations of `scope` — would be two answers to the same question. Pure,
/// like everything else in this module.
///
/// **The digest describes the file, not the framed blob.** That is deliberate
/// and was warned about in advance (the instructions ADR §7): the digest's job
/// is to prove which file in the repository contributed, and a digest taken
/// over the framing would change whenever the preamble was reworded, proving
/// nothing about the repository.
pub fn system_prompt(
    set: &InstructionSet,
    root: &Path,
    selection: &Selection,
) -> Option<SystemPrompt> {
    let sources = descriptors(set, root, selection);
    if sources.is_empty() {
        return None;
    }

    let mut text = String::from(PREAMBLE);
    for (instruction, descriptor) in set.loaded.iter().zip(&sources) {
        text.push_str("\n\n# ");
        text.push_str(&descriptor.path);
        text.push_str("\n\n");
        text.push_str(instruction.text.trim_end());
    }

    Some(SystemPrompt { text, sources })
}

/// What a run loaded, in the terms `PublicEvent::SessionStarted` publishes:
/// path, digest, size, scope and how it was chosen — and no text.
///
/// Private, and reached only through [`system_prompt`], so that a descriptor
/// cannot be published for a file whose text was not also sent.
///
/// `scope` is the workspace-relative directory the file governs, `"."` at the
/// root, so the root→cwd ordering is legible without re-deriving it from the
/// paths. A file outside `root` — which only an explicitly named one can be —
/// has no relative scope and reports its own directory instead.
fn descriptors(
    set: &InstructionSet,
    root: &Path,
    selection: &Selection,
) -> Vec<InstructionDescriptor> {
    let source = match selection {
        Selection::Explicit(_) => InstructionSource::Explicit,
        Selection::Ambient | Selection::Disabled => InstructionSource::Ambient,
    };
    set.loaded
        .iter()
        .map(|instruction| InstructionDescriptor {
            path: instruction.path.display().to_string(),
            sha256: instruction.digest.clone(),
            bytes: instruction.bytes,
            scope: scope_of(&instruction.path, root),
            source,
        })
        .collect()
}

/// The directory `path`'s file governs, relative to `root`.
fn scope_of(path: &Path, root: &Path) -> String {
    let dir = path.parent().unwrap_or(path);
    match dir.strip_prefix(root) {
        // The root's own file governs the whole workspace, and `strip_prefix`
        // leaves an empty path for it. `"."` is that directory's name.
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
        Ok(relative) => relative.display().to_string(),
        Err(_) => dir.display().to_string(),
    }
}

/// Lowercase hex SHA-256, from the one place that spells it.
fn digest_of(bytes: &[u8]) -> String {
    crate::digest::sha256_hex(bytes)
}
