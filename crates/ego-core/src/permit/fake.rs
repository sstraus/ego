//! `FakeFs`: an in-memory `Fs` for tests, next to the seam it exercises.
//!
//! Ships in `src` rather than under `tests/` for the reason
//! [`generator::fake`](crate::generator::fake) does: more than one crate's
//! tests need it, and a fake copied into each of them is a fake that drifts
//! from the trait it stands in for. It is also the only `Fs` that can change
//! its answer between two calls, which is how the TOCTOU tests are written.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;

use super::fs::{Fs, FsError, GuardedIo, PathResolver};
use super::path::CanonPath;
use super::prepared_call::{Change, Op, PreparedCall};
use super::scan::{Entry, EntryKind, Scan, ScanRequest};
use super::tool::{PrepareCtx, ToolError, ToolResult, TypedTool};

/// An in-memory, test-only `Fs`: a flat table from path to either file
/// bytes or a symlink target, resolved by whole-path substitution (not
/// POSIX component-by-component — this fake only needs to model "what
/// does this path resolve to right now", including changing its answer
/// between two calls, which is exactly the TOCTOU this story guards
/// against).
#[derive(Default)]
pub struct FakeFs {
    table: Mutex<HashMap<PathBuf, Node>>,
    /// How many times each seam was actually entered.
    ///
    /// "The guard refuses before opening anything" is a claim about work that
    /// did *not* happen, and the only way to assert it is to count. Without
    /// these, a guard that walked the tree and then discarded the results
    /// would pass every test that only inspects the return value.
    scans: AtomicUsize,
    reads: AtomicUsize,
}

enum Node {
    File(Vec<u8>),
    Symlink(PathBuf),
}

impl FakeFs {
    /// Points `from` at `to`, overwriting any previous entry. Callable
    /// at any time — including between a tool's `prepare` and
    /// `execute` — which is how tests express the symlink race.
    pub fn symlink(&self, from: &str, to: &str) {
        self.table
            .lock()
            .unwrap()
            .insert(PathBuf::from(from), Node::Symlink(PathBuf::from(to)));
    }

    /// How many traversals this fs has performed.
    pub fn scans(&self) -> usize {
        self.scans.load(Ordering::SeqCst)
    }

    /// How many file reads this fs has performed.
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    /// Puts `bytes` at `path`, so a read tool has something to find.
    pub fn file(&self, path: &str, bytes: &str) {
        self.table
            .lock()
            .unwrap()
            .insert(PathBuf::from(path), Node::File(bytes.as_bytes().to_vec()));
    }
}

impl PathResolver for FakeFs {
    /// Resolves symlinks, and succeeds for a path it has never heard of.
    ///
    /// That is the trait's job — "ask what this path currently resolves to" —
    /// and not existence, which is [`Fs::read`]'s. A real `OsFs` also reports
    /// `NotFound` here, because `std::fs::canonicalize` cannot resolve what is
    /// not on disk; that half of the contract is exercised against the real
    /// filesystem rather than faked here, where "the table has no row" and
    /// "the file does not exist" are not the same claim.
    fn canonicalize(&self, path: &Path) -> Result<CanonPath, FsError> {
        let table = self.table.lock().unwrap();
        let mut current = path.to_path_buf();
        for _ in 0..32 {
            match table.get(&current) {
                Some(Node::Symlink(target)) => current = target.clone(),
                _ => return Ok(CanonPath::new(current)),
            }
        }
        // A loop is a typed error, not a panic: a filesystem a user controls
        // can contain one, and a fake that aborts the process on it cannot be
        // used to test what the code above does about it.
        Err(FsError::Loop {
            path: CanonPath::new(path),
        })
    }
}

impl Fs for FakeFs {
    fn read(&self, path: &CanonPath) -> Result<Vec<u8>, FsError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        match self.table.lock().unwrap().get(path.as_path()) {
            Some(Node::File(bytes)) => Ok(bytes.clone()),
            Some(Node::Symlink(_)) => Err(FsError::NotAFile { path: path.clone() }),
            None => Err(FsError::NotFound { path: path.clone() }),
        }
    }

    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), FsError> {
        self.table
            .lock()
            .unwrap()
            .insert(path.as_path().to_path_buf(), Node::File(bytes.to_vec()));
        Ok(())
    }

    /// Walks the flat table as if it were a tree.
    ///
    /// Honours the same three obligations a real implementation must: it does
    /// not descend into a directory symlink, it stops at every
    /// [`ScanLimits`](crate::permit::ScanLimits) bound and says so, and it
    /// refuses to descend into a name in `skip_dirs`. A fake that took the
    /// easy way out on any of the three would let the guarded seam's tests
    /// pass against behaviour no real filesystem provides.
    fn scan(&self, req: &ScanRequest) -> Result<Scan, FsError> {
        self.scans.fetch_add(1, Ordering::SeqCst);
        let table = self.table.lock().unwrap();
        let root = req.root.as_path();

        // Sorted, so a scan is deterministic and a truncated one truncates
        // the same way twice.
        // Keyed by the path as it appears under the root, valued by the
        // canonical target — the two differ exactly when the entry is a
        // symlink, which is the case the permit check has to see through.
        let mut files: BTreeSet<(PathBuf, PathBuf)> = BTreeSet::new();
        let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
        for (path, node) in table.iter() {
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            if relative.as_os_str().is_empty() {
                continue;
            }
            match node {
                Node::File(_) => {
                    files.insert((path.clone(), path.clone()));
                }
                Node::Symlink(target) => match table.get(target) {
                    // A symlinked *file* is resolved and reported under its
                    // canonical target, so an escape out of the root is
                    // something the permit check can see. A link to anything
                    // else — a directory, or nothing — is neither descended
                    // into nor reported, which is the no-follow rule and is
                    // why this fake cannot produce a cycle.
                    Some(Node::File(_)) => {
                        files.insert((path.clone(), target.clone()));
                    }
                    _ => continue,
                },
            }
            for ancestor in relative.ancestors().skip(1) {
                if ancestor.as_os_str().is_empty() {
                    break;
                }
                dirs.insert(root.join(ancestor));
            }
        }

        let mut entries = Vec::new();
        let mut truncated = false;
        let candidates = dirs
            .into_iter()
            .map(|path| (path.clone(), path, EntryKind::Dir))
            .chain(
                files
                    .into_iter()
                    .map(|(at, target)| (at, target, EntryKind::File)),
            );

        for (at, target, kind) in candidates {
            let relative = at
                .strip_prefix(root)
                .expect("every candidate was collected under the root")
                .to_path_buf();

            if relative
                .components()
                .any(|c| req.skips(&c.as_os_str().to_string_lossy()))
            {
                continue;
            }
            // `max_depth: 0` means the root's own children and nothing below,
            // so a one-component relative path is depth 0.
            if relative.components().count().saturating_sub(1) > req.limits.max_depth {
                truncated = true;
                continue;
            }
            if entries.len() >= req.limits.max_entries {
                truncated = true;
                break;
            }
            entries.push(Entry {
                relative,
                path: CanonPath::new(target),
                kind,
            });
        }

        Ok(Scan::new(entries, truncated))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteParams {
    pub path: String,
    pub contents: String,
}

/// A fake builtin tool: writes `contents` to `path`, going through
/// `GuardedIo` exactly like a real tool must — it never sees a `Fs`.
pub struct FakeWriteTool;

impl TypedTool for FakeWriteTool {
    type Params = WriteParams;

    fn name(&self) -> &str {
        "fake_write"
    }

    fn description(&self) -> &str {
        "writes contents to a file, for testing the permit seam"
    }

    fn prepare_typed(
        &self,
        params: WriteParams,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        let resolved = ctx.resolver.canonicalize(Path::new(&params.path))?;
        Ok(PreparedCall::new(
            self.name(),
            vec![Op::Write {
                path: resolved,
                change: Change::Whole(params.contents.into_bytes()),
            }],
        ))
    }

    fn execute<'a>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let Some(Op::Write {
                path,
                change: Change::Whole(contents),
            }) = call.ops.into_iter().next()
            else {
                return Err(ToolError::InvalidParams(
                    "expected one whole Write op".into(),
                ));
            };
            io.write(&path, &contents)?;
            Ok(ToolResult::builtin(format!(
                "wrote {} bytes to {path}",
                contents.len()
            )))
        })
    }
}
