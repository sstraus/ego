//! `OsFs`: the [`Fs`] a model's tools actually run against.
//!
//! The only *model-controlled* filesystem adapter in the workspace — the
//! session store touches `std::fs` too, but behind a seam no tool can reach
//! (`ARCHITECTURE.md` §7).
//! Nothing here consults a `Permit`: this is the raw seam, held by the turn
//! loop and wrapped in `GuardedFs` before a tool ever sees it.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use ego_core::permit::{CanonPath, Entry, EntryKind, Fs, FsError, PathResolver, Scan, ScanRequest};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use tempfile::NamedTempFile;

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?
            .sync_all()
    }

    #[cfg(not(windows))]
    fs::File::open(path)?.sync_all()
}

/// `std::fs`, behind the port.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsFs;

/// True when the platform gave up chasing symlinks.
///
/// `std::io::ErrorKind::FilesystemLoop` is the name for this and is still
/// unstable (rust#86442), so the errno is read directly. `ELOOP` is frozen in
/// each platform's ABI — it cannot drift the way an `ErrorKind` mapping can —
/// and the test that builds a real symlink cycle is what proves the value we
/// were compiled against.
#[cfg(unix)]
fn is_symlink_loop(err: &io::Error) -> bool {
    const ELOOP: i32 = if cfg!(target_os = "linux") { 40 } else { 62 };
    err.raw_os_error() == Some(ELOOP)
}

/// Nothing to recognise: a platform whose `ELOOP` this code does not know
/// reports the loop as [`FsError::Io`], carrying the message the OS gave.
/// Guessing would be worse than quoting.
#[cfg(not(unix))]
fn is_symlink_loop(_err: &io::Error) -> bool {
    false
}

impl OsFs {
    /// Translate what the OS said into what a model can act on.
    ///
    /// The four named variants are the four different next moves: look
    /// elsewhere, read it as a directory instead, walk it as a file instead,
    /// or stop chasing the link. Everything else keeps the platform's own
    /// words rather than being flattened into one of them.
    fn error(path: &Path, err: &io::Error) -> FsError {
        let path = CanonPath::new(path);
        match err.kind() {
            io::ErrorKind::NotFound => FsError::NotFound { path },
            io::ErrorKind::IsADirectory => FsError::NotAFile { path },
            io::ErrorKind::NotADirectory => FsError::NotADirectory { path },
            _ if is_symlink_loop(err) => FsError::Loop { path },
            _ => FsError::Io {
                path,
                message: err.to_string(),
            },
        }
    }
}

impl PathResolver for OsFs {
    fn canonicalize(&self, path: &Path) -> Result<CanonPath, FsError> {
        fs::canonicalize(path)
            .map(CanonPath::new)
            .map_err(|err| Self::error(path, &err))
    }
}

/// A complete replacement staged beside its target but not yet visible at
/// the target's name.
///
/// The type is the deterministic boundary between preparation and commit:
/// dropping it removes the [`NamedTempFile`] and leaves the target unchanged;
/// `commit` is the only operation that publishes the new inode.
struct PreparedReplacement {
    target: PathBuf,
    parent: PathBuf,
    temp: NamedTempFile,
}

impl PreparedReplacement {
    fn new(target: &Path, bytes: &[u8]) -> io::Result<Self> {
        let parent = target.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a file target must have a parent",
            )
        })?;

        // The temporary file is exclusive, unpredictably named and on the
        // target's filesystem. Unlike StoreIo's recovery scratch, its name is
        // not repository state and cannot collide by construction.
        let permissions = match fs::metadata(target) {
            Ok(metadata) => Some(metadata.permissions()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        };
        let mut temp = NamedTempFile::new_in(parent)?;

        temp.write_all(bytes)?;
        if let Some(permissions) = permissions {
            // Apply the final mode after writing: a read-only target still
            // needs to be replaceable by a caller that may replace it, and
            // the open scratch handle must not be made unwritable midway.
            temp.as_file().set_permissions(permissions)?;
        }
        // The rename may become durable before dirty data pages do. Sync the
        // complete replacement first so the published inode is never an
        // atomically installed empty or partial file after a crash.
        temp.as_file().sync_all()?;

        Ok(Self {
            target: target.to_path_buf(),
            parent: parent.to_path_buf(),
            temp,
        })
    }

    fn commit(self) -> io::Result<()> {
        let Self {
            target,
            parent,
            temp,
        } = self;

        // `persist` is one atomic replacement. On error it returns ownership
        // of the NamedTempFile; converting the PersistError to io::Error drops
        // that file and therefore removes the scratch path.
        let persisted = temp.persist(&target).map_err(io::Error::from)?;
        drop(persisted);

        // The file data is already stable. This barrier makes the directory
        // entry that names it stable before success is reported.
        sync_directory(&parent)
    }
}

impl Fs for OsFs {
    fn read(&self, path: &CanonPath) -> Result<Vec<u8>, FsError> {
        fs::read(path.as_path()).map_err(|err| Self::error(path.as_path(), &err))
    }

    fn read_bounded(&self, path: &CanonPath, limit: usize) -> Result<Vec<u8>, FsError> {
        use std::io::Read;
        for component in path.as_path().ancestors() {
            let metadata =
                fs::symlink_metadata(component).map_err(|e| Self::error(component, &e))?;
            if metadata.file_type().is_symlink() {
                return Err(FsError::Io {
                    path: path.clone(),
                    message: "command import crosses a symlink".into(),
                });
            }
        }
        let file = fs::File::open(path.as_path()).map_err(|e| Self::error(path.as_path(), &e))?;
        if !file
            .metadata()
            .map_err(|e| Self::error(path.as_path(), &e))?
            .is_file()
        {
            return Err(FsError::NotAFile { path: path.clone() });
        }
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Self::error(path.as_path(), &e))?;
        if bytes.len() > limit {
            return Err(FsError::Io {
                path: path.clone(),
                message: "file exceeds import byte limit".into(),
            });
        }
        Ok(bytes)
    }

    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), FsError> {
        PreparedReplacement::new(path.as_path(), bytes)
            .and_then(PreparedReplacement::commit)
            .map_err(|err| Self::error(path.as_path(), &err))
    }

    fn scan(&self, req: &ScanRequest) -> Result<Scan, FsError> {
        let mut walk = Walk {
            req,
            entries: Vec::new(),
            truncated: false,
            ignores: if req.respect_ignore {
                repository_ignores(req.root.as_path())
            } else {
                Vec::new()
            },
        };
        // The root's own failure is the caller's problem — a scan rooted at a
        // file is `NotADirectory`, and there is no partial answer to give.
        walk.descend(req.root.as_path(), Path::new(""), 0)?;
        Ok(Scan::new(walk.entries, walk.truncated))
    }
}

/// One traversal in progress.
struct Walk<'a> {
    req: &'a ScanRequest,
    entries: Vec<Entry>,
    truncated: bool,
    /// The `.gitignore` files in force, outermost first.
    ignores: Vec<Gitignore>,
}

/// The `.gitignore` in `dir`, if it has one that parses.
fn gitignore_in(dir: &Path) -> Option<Gitignore> {
    let file = dir.join(".gitignore");
    if !file.is_file() {
        return None;
    }
    let mut builder = GitignoreBuilder::new(dir);
    // A line that does not parse is skipped by the builder; the rest still apply.
    builder.add(file);
    builder.build().ok()
}

/// The ignore files above `root`, outermost first, up to the repository that
/// holds it. Outside a repository, or when `root` is itself a repository,
/// nothing above applies.
fn repository_ignores(root: &Path) -> Vec<Gitignore> {
    let Some(top) = root.ancestors().find(|dir| dir.join(".git").exists()) else {
        return Vec::new();
    };
    let mut above: Vec<Gitignore> = root
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(top))
        .filter_map(gitignore_in)
        .collect();
    above.reverse();
    above
}

impl Walk<'_> {
    /// Visit everything directly inside `dir`, and recurse where allowed.
    ///
    /// `depth` counts levels already descended below the root, so the entries
    /// this call yields are at `depth`, and `max_depth: 0` means the root's own
    /// children and nothing deeper.
    ///
    /// Failure is not uniform, and deliberately so. This returns `Err` for the
    /// directory it was asked to open, but a *nested* directory it cannot open
    /// is skipped and logged: one unreadable subdirectory is a fact about that
    /// subdirectory, and abandoning the whole walk over it would make `glob`
    /// unusable in any tree containing a root-owned directory. Only the root's
    /// failure is fatal, because there is no partial answer to give when the
    /// thing you were pointed at is not walkable at all.
    fn descend(&mut self, dir: &Path, relative: &Path, depth: usize) -> Result<(), FsError> {
        // A repository nested below the root answers to its own rules only.
        let outer = (self.req.respect_ignore
            && !relative.as_os_str().is_empty()
            && dir.join(".git").exists())
        .then(|| std::mem::take(&mut self.ignores));
        let own = self.req.respect_ignore.then(|| gitignore_in(dir)).flatten();
        let scoped = own.is_some();
        self.ignores.extend(own);
        let walked = self.visit(dir, relative, depth);
        if scoped {
            self.ignores.pop();
        }
        if let Some(outer) = outer {
            self.ignores = outer;
        }
        walked
    }

    /// True when the nearest `.gitignore` with an opinion ignores `at`.
    fn ignored(&self, at: &Path, is_dir: bool) -> bool {
        self.ignores
            .iter()
            .rev()
            .find_map(|rules| match rules.matched(at, is_dir) {
                Match::None => None,
                Match::Ignore(_) => Some(true),
                Match::Whitelist(_) => Some(false),
            })
            .unwrap_or(false)
    }

    fn visit(&mut self, dir: &Path, relative: &Path, depth: usize) -> Result<(), FsError> {
        // Read the names first and sort them: `read_dir` yields whatever order
        // the filesystem feels like, and a scan that truncates has to truncate
        // the same way twice or a model gets a different answer to the same
        // question.
        let mut names: Vec<OsString> = Vec::new();
        for entry in fs::read_dir(dir).map_err(|err| OsFs::error(dir, &err))? {
            match entry {
                Ok(entry) => names.push(entry.file_name()),
                Err(err) => tracing::warn!(
                    dir = %dir.display(),
                    %err,
                    "a directory entry could not be read and was skipped"
                ),
            }
        }
        names.sort();

        // Visit files before entering sibling directories: a large early
        // subtree must not spend the scan or search budget before root files.
        let mut directories = Vec::new();
        for name in names {
            if self.entries.len() >= self.req.limits.max_entries {
                self.truncated = true;
                return Ok(());
            }

            let at = dir.join(&name);
            let child = relative.join(&name);
            // `symlink_metadata`, never `metadata`: the question here is what
            // this entry *is*, not what it points at.
            let Ok(meta) = fs::symlink_metadata(&at) else {
                // Deleted between the listing and the stat, or refused. An
                // entry that cannot be described is not reported.
                continue;
            };
            if self.ignored(&at, meta.is_dir()) {
                continue;
            }

            if meta.is_symlink() {
                self.link(child, &at);
            } else if meta.is_dir() {
                directories.push((child, at, name));
            } else if meta.is_file() {
                self.entries.push(Entry {
                    relative: child,
                    path: CanonPath::new(at),
                    kind: EntryKind::File,
                });
            }
            // A socket, fifo or device is neither a file to read nor a
            // directory to walk, and reporting one as either would be a lie
            // about what the next operation can do with it.
        }
        for (child, at, name) in directories {
            if self.entries.len() >= self.req.limits.max_entries {
                self.truncated = true;
                return Ok(());
            }
            self.directory(child, &at, &name, depth);
        }
        Ok(())
    }

    /// A symlink: resolved, reported if it lands on a file, and never
    /// descended into.
    ///
    /// This is the no-follow rule (`ARCHITECTURE.md`
    /// §4). A walk resolves many paths where a read resolves one, so a
    /// symlinked directory inside an in-scope root is an escape the root check
    /// cannot see; refusing to enter one closes it and makes cycles impossible
    /// rather than merely detectable. A symlinked *file* is still reported —
    /// under its canonical target, so an escape is something the permit check
    /// can see and refuse.
    fn link(&mut self, relative: std::path::PathBuf, at: &Path) {
        let Ok(target) = fs::canonicalize(at) else {
            return; // dangling, or a loop
        };
        if fs::metadata(&target).is_ok_and(|meta| meta.is_file()) {
            self.entries.push(Entry {
                relative,
                path: CanonPath::new(target),
                kind: EntryKind::File,
            });
        }
    }

    /// A real directory: skipped, reported, or reported and walked.
    fn directory(
        &mut self,
        relative: std::path::PathBuf,
        at: &Path,
        name: &OsString,
        depth: usize,
    ) {
        // A skipped directory is not reported either. `skip_dirs` exists so
        // `target/` stays out of a glob's results, and a result listing the
        // directory it refused to look inside serves nobody. It is not a
        // security boundary: everything under it is still refused by the
        // permit exactly like any other path.
        if self.req.skips(&name.to_string_lossy()) {
            return;
        }
        self.entries.push(Entry {
            relative: relative.clone(),
            path: CanonPath::new(at),
            kind: EntryKind::Dir,
        });

        if depth >= self.req.limits.max_depth {
            // Truncated even though this directory might have been empty:
            // finding that out means opening it, which is the one thing the
            // limit forbids. The only answer available without doing the
            // forbidden I/O is "the walk stopped here", and a model that is
            // told a listing is complete when it is not concludes the file it
            // needs does not exist.
            self.truncated = true;
            return;
        }
        if let Err(err) = self.descend(at, &relative, depth + 1) {
            tracing::warn!(
                dir = %at.display(),
                %err,
                "a directory could not be walked and was skipped"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Preparing a replacement is allowed to create and fill a private
    /// scratch file, but the named target must remain untouched until commit.
    /// Dropping an uncommitted preparation models every failure before
    /// persistence without scheduling a reader against a writer.
    #[test]
    fn an_uncommitted_replacement_keeps_the_old_target_and_cleans_its_scratch() {
        let dir = TempDir::new().expect("a temp directory");
        let target = dir.path().join("target.txt");
        fs::write(&target, b"old").expect("the target is created");
        let mut scratch = None;

        let failed = (|| -> io::Result<()> {
            let prepared = PreparedReplacement::new(&target, b"new")?;
            scratch = Some(prepared.temp.path().to_path_buf());

            assert_eq!(fs::read(&target).unwrap(), b"old");
            assert!(
                scratch.as_ref().is_some_and(|path| path.exists()),
                "the prepared bytes live beside the target"
            );

            Err(io::Error::other("injected failure before commit"))
        })();

        assert!(failed.is_err(), "the pre-commit failure was injected");
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(
            scratch.as_ref().is_some_and(|path| !path.exists()),
            "returning the error drops the preparation and cleans its scratch"
        );
    }

    #[test]
    fn a_committed_replacement_publishes_complete_bytes() {
        let dir = TempDir::new().expect("a temp directory");
        let target = dir.path().join("target.txt");

        PreparedReplacement::new(&target, b"complete")
            .expect("the replacement is prepared")
            .commit()
            .expect("the replacement is durably committed");

        assert_eq!(fs::read(target).unwrap(), b"complete");
    }
}
