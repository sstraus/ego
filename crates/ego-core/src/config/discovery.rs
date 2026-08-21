//! Discovering the instructions a workspace hands a model.
//!
//! `AGENTS.md`, from the workspace root down to the current directory, only in
//! a workspace the trust gate let through. See
//! `docs/decisions/2026-08-21-instructions-a-workspace-may-hand-a-model.md`
//! and `docs/08-codex-critique.md` I2.
//!
//! Split the way everything in this crate is split: the *decision* is a pure
//! function here, and the reading happens behind [`InstructionIo`], whose
//! only real implementation lives in `ego-tools`. Root→cwd needs no directory
//! walk — the filename is known, so the path list is arithmetic over the
//! caller's already-canonical inputs.
//!
//! ## What this does not do
//!
//! It loads; it does not inject. Nothing here reaches `GenRequest`, and
//! nothing here is persisted: the descriptors are returned so that the harness
//! that assembles a prompt and writes `PublicEvent::SessionStarted` can carry
//! them. Instructions are **declarative** — they may shape what a model is
//! told, and they can never grant a permission or run a hook.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::resolve::Posture;

/// The default file every ancestor directory is checked for.
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
    /// [`INSTRUCTIONS_FILE`], workspace root down to cwd.
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
    /// [`Limits::max_files`] files had already been loaded.
    TooManyFiles { limit: usize },
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
                write!(f, "{limit} instruction files had already been loaded")
            }
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
    /// Read `path`, refusing anything that is not a regular non-symlinked
    /// file of at most `max_bytes`.
    ///
    /// An implementation must not follow a symlink at the final component,
    /// and must not read more than `max_bytes + 1` — one byte past the limit
    /// is what distinguishes "exactly at it" from "over it" without pulling
    /// an arbitrarily large file into memory.
    fn load(&self, path: &Path, max_bytes: u64) -> Found;
}

/// One loaded instruction file.
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
    let paths = plan(root, cwd, selection)?;
    let mut set = InstructionSet::default();
    let mut total: u64 = 0;

    for path in paths {
        if set.loaded.len() >= limits.max_files {
            set.skipped.push(Skipped {
                path,
                reason: Refusal::TooManyFiles {
                    limit: limits.max_files,
                },
            });
            continue;
        }

        match io.load(&path, limits.max_file_bytes) {
            Found::Absent => {}
            Found::Refused(reason) => set.skipped.push(Skipped { path, reason }),
            Found::Read { bytes } => {
                let len = bytes.len() as u64;
                if total + len > limits.max_total_bytes {
                    set.skipped.push(Skipped {
                        path,
                        reason: Refusal::TotalTooLarge {
                            limit: limits.max_total_bytes,
                        },
                    });
                    continue;
                }
                match String::from_utf8(bytes) {
                    Err(_) => set.skipped.push(Skipped {
                        path,
                        reason: Refusal::NotText,
                    }),
                    Ok(text) => {
                        total += len;
                        set.loaded.push(Instruction {
                            path,
                            digest: digest_of(text.as_bytes()),
                            bytes: len,
                            text,
                        });
                    }
                }
            }
        }
    }

    Ok(set)
}

/// Lowercase hex SHA-256.
fn digest_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
