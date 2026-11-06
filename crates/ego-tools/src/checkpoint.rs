//! Session-owned Git objects for builtin writes. Never use the user's index.
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ego_core::event::id::SessionId;
use ego_core::permit::{CanonPath, Fs, FsError, FsWriteAttempt, PathResolver, Scan, ScanRequest};
use serde::{Deserialize, Serialize};

use crate::fs::{OsFs, PreparedReplacement, sync_directory};

#[derive(Debug)]
pub struct CheckpointFs {
    session: SessionId,
    enabled: bool,
    /// The checkpoint the last write of each path created, for its result.
    last: std::sync::Mutex<std::collections::HashMap<PathBuf, String>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    path: PathBuf,
    existed: bool,
    mode: u32,
    preimage_identity: Option<FileIdentity>,
    postimage_identity: FileIdentity,
    restoration: bool,
}

/// Captured from the staged atomic replacement before the ref is published.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct FileIdentity {
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileIdentity {
    fn of(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            created: metadata.created().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Preview {
    pub checkpoint: String,
    pub path: PathBuf,
    pub effect: &'static str,
    pub conflict: bool,
    pub already_original: bool,
}

/// One checkpoint in the session's chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub checkpoint: String,
    pub path: PathBuf,
    pub restoration: bool,
}

struct Repository {
    root: PathBuf,
    reference: String,
}

fn error(message: impl ToString) -> io::Error {
    io::Error::other(message.to_string())
}

// No porcelain, filters, hooks, replacement objects, or inherited alternate index.
fn git(root: &Path, args: &[&str], input: &[u8]) -> io::Result<Vec<u8>> {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    let mut child = command
        .arg("--no-replace-objects")
        .args([
            "-c",
            "core.fsync=all",
            "-c",
            "core.hooksPath=/dev/null",
            "-C",
        ])
        .arg(root)
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "ego checkpoints")
        .env("GIT_AUTHOR_EMAIL", "checkpoints@ego.invalid")
        .env("GIT_COMMITTER_NAME", "ego checkpoints")
        .env("GIT_COMMITTER_EMAIL", "checkpoints@ego.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let written = child.stdin.take().expect("piped stdin").write_all(input);
    let output = child.wait_with_output()?;
    written?;
    if !output.status.success() {
        return Err(error(format!(
            "checkpoint Git operation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn text(bytes: Vec<u8>) -> io::Result<String> {
    String::from_utf8(bytes)
        .map(|s| s.strip_suffix('\n').unwrap_or(&s).to_owned())
        .map_err(error)
}

fn mode(path: &Path) -> io::Result<u32> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(error("checkpoint target must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(metadata.permissions().mode())
    }
    #[cfg(not(unix))]
    Ok(u32::from(metadata.permissions().readonly()))
}

impl Repository {
    fn discover(directory: &Path, session: SessionId) -> io::Result<Option<Self>> {
        let canonical_directory = fs::canonicalize(directory)?;
        let directory = canonical_directory.as_path();
        // Outside Git does not require a Git installation. Do not let an inherited
        // GIT_DIR assign ownership to an unrelated repository.
        let mut has_metadata = false;
        for ancestor in directory.ancestors() {
            match fs::symlink_metadata(ancestor.join(".git")) {
                Ok(_) => {
                    has_metadata = true;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        if !has_metadata {
            return Ok(None);
        }
        // A missing Git executable is not the same as an unversioned directory.
        let mut command = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        let output = command
            .args(["-C"])
            .arg(directory)
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        if !output.status.success() {
            return Err(error("cannot discover checkpoint repository"));
        }
        Ok(Some(Self {
            root: fs::canonicalize(text(output.stdout)?)?,
            reference: format!("refs/ego/checkpoints/{session}"),
        }))
    }

    fn tip(&self) -> io::Result<Option<String>> {
        // show-ref's exit 1 is also used for an absent ref in an unborn repository.
        let result = git(
            &self.root,
            &["rev-parse", "--verify", "--quiet", &self.reference],
            &[],
        );
        match result {
            Ok(bytes) => Ok(Some(text(bytes)?)),
            Err(_) => {
                let refs = text(git(
                    &self.root,
                    &["for-each-ref", "--format=%(refname)", &self.reference],
                    &[],
                )?)?;
                if refs.lines().any(|r| r == self.reference) {
                    Err(error("invalid checkpoint ref"))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn blob(&self, bytes: &[u8]) -> io::Result<String> {
        text(git(&self.root, &["hash-object", "-w", "--stdin"], bytes)?)
    }

    fn lock(&self) -> io::Result<fd_lock::RwLock<fs::File>> {
        let common = text(git(&self.root, &["rev-parse", "--git-common-dir"], &[])?)?;
        let directory = self.root.join(common).join("ego-checkpoints");
        fs::create_dir_all(&directory)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("lock"))?;
        Ok(fd_lock::RwLock::new(file))
    }

    fn checked_path(&self, relative: &Path) -> io::Result<PathBuf> {
        if relative.as_os_str().is_empty() || relative.components().any(|c| !matches!(c, std::path::Component::Normal(name) if !name.to_string_lossy().eq_ignore_ascii_case(".git"))) {
            return Err(error("invalid checkpoint path"));
        }
        let path = self.root.join(relative);
        let parent = path.parent().ok_or_else(|| error("missing parent"))?;
        if fs::canonicalize(parent)? != parent {
            return Err(error("checkpoint restore crosses a symlink"));
        }
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(error("checkpoint target is a symlink"));
        }
        Ok(path)
    }

    fn capture(
        &self,
        path: &Path,
        after: &[u8],
        replacement: &PreparedReplacement,
        restoration: bool,
    ) -> io::Result<String> {
        let relative = path.strip_prefix(&self.root).map_err(error)?;
        self.checked_path(relative)?;
        let (before, existed) = match fs::read(path) {
            Ok(bytes) => (bytes, true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (Vec::new(), false),
            Err(e) => return Err(e),
        };
        let staged = replacement.metadata()?;
        #[cfg(unix)]
        let permissions = {
            use std::os::unix::fs::PermissionsExt;
            staged.permissions().mode()
        };
        #[cfg(not(unix))]
        let permissions = u32::from(staged.permissions().readonly());
        let record = Record {
            path: relative.to_owned(),
            existed,
            mode: permissions,
            preimage_identity: if existed {
                Some(FileIdentity::of(&fs::symlink_metadata(path)?))
            } else {
                None
            },
            postimage_identity: FileIdentity::of(&staged),
            restoration,
        };
        let before = self.blob(&before)?;
        let after = self.blob(after)?;
        let metadata = self.blob(&serde_json::to_vec(&record)?)?;
        let tree = text(git(&self.root, &["mktree"], format!("100644 blob {after}\tafter\n100644 blob {before}\tbefore\n100644 blob {metadata}\trecord\n").as_bytes())?)?;
        let tip = self.tip()?;
        let mut args = vec!["commit-tree", tree.as_str()];
        if let Some(parent) = tip.as_deref() {
            args.extend(["-p", parent]);
        }
        let commit = text(git(&self.root, &args, b"ego builtin write checkpoint\n")?)?;
        let zero = "0".repeat(commit.len());
        git(
            &self.root,
            &[
                "update-ref",
                &self.reference,
                &commit,
                tip.as_deref().unwrap_or(&zero),
            ],
            &[],
        )?;
        Ok(commit)
    }

    fn ids(&self) -> io::Result<Vec<String>> {
        if self.tip()?.is_none() {
            return Ok(Vec::new());
        }
        Ok(text(git(&self.root, &["rev-list", &self.reference], &[])?)?
            .lines()
            .map(str::to_owned)
            .collect())
    }

    fn load(&self, id: &str) -> io::Result<(Record, Vec<u8>, Vec<u8>)> {
        if !matches!(id.len(), 40 | 64)
            || !id.bytes().all(|b| b.is_ascii_hexdigit())
            || !self.ids()?.iter().any(|s| s == id)
        {
            return Err(error("checkpoint does not belong to this session"));
        }
        let read = |name| git(&self.root, &["show", &format!("{id}:{name}")], &[]);
        Ok((self.record(id)?, read("before")?, read("after")?))
    }

    fn record(&self, id: &str) -> io::Result<Record> {
        Ok(serde_json::from_slice(&git(
            &self.root,
            &["show", &format!("{id}:record")],
            &[],
        )?)?)
    }

    fn owns_restored_identity(&self, path: &Path, identity: &FileIdentity) -> io::Result<bool> {
        // A preceding undo atomically installs a new inode. Its durable intent is
        // the ownership evidence that lets the next older undo accept that inode.
        for id in self.ids()? {
            let record = self.record(&id)?;
            if record.restoration && record.path == path && &record.postimage_identity == identity {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn preview(&self, id: &str) -> io::Result<Preview> {
        let (record, before, after) = self.load(id)?;
        let path = match self.checked_path(&record.path) {
            Ok(path) => path,
            Err(_) => {
                return Ok(Preview {
                    checkpoint: id.to_owned(),
                    path: record.path,
                    effect: if record.existed { "replace" } else { "delete" },
                    conflict: true,
                    already_original: false,
                });
            }
        };
        let current = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let original = if record.existed {
            current.as_ref() == Some(&before) && mode(&path)? == record.mode
        } else {
            current.is_none()
        };
        let mode_matches = current.is_some() && mode(&path)? == record.mode;
        let identity_matches = if current.is_some() {
            let identity = FileIdentity::of(&fs::symlink_metadata(&path)?);
            identity == record.postimage_identity
                || self.owns_restored_identity(&record.path, &identity)?
        } else {
            false
        };
        Ok(Preview {
            checkpoint: id.to_owned(),
            path: record.path,
            effect: if record.existed { "replace" } else { "delete" },
            conflict: !original
                && (current.as_ref() != Some(&after) || !mode_matches || !identity_matches),
            already_original: original,
        })
    }
}

impl CheckpointFs {
    pub fn new(session: SessionId) -> Self {
        Self {
            session,
            enabled: true,
            last: std::sync::Mutex::default(),
        }
    }

    /// Whether writes save a checkpoint first. Off, a write is a plain atomic write: no
    /// Git object and no ref is created in the user's repository.
    #[must_use]
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn preview(directory: &Path, session: SessionId) -> io::Result<Vec<Preview>> {
        let repo = Repository::discover(directory, session)?
            .ok_or_else(|| error("no checkpoints outside Git"))?;
        let mut lock = repo.lock()?;
        let _guard = lock.write()?;
        repo.ids()?
            .iter()
            .filter_map(|id| match repo.record(id) {
                Ok(record) if record.restoration => None,
                Ok(_) => Some(repo.preview(id)),
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    /// Every checkpoint of the session, newest first, with its path and
    /// whether it records a restore rather than a builtin write.
    pub fn history(directory: &Path, session: SessionId) -> io::Result<Vec<HistoryEntry>> {
        let Some(repo) = Repository::discover(directory, session)? else {
            return Ok(Vec::new());
        };
        let mut lock = repo.lock()?;
        let _guard = lock.write()?;
        repo.ids()?
            .into_iter()
            .map(|id| {
                let record = repo.record(&id)?;
                Ok(HistoryEntry {
                    checkpoint: id,
                    path: record.path,
                    restoration: record.restoration,
                })
            })
            .collect()
    }

    /// The Git blob ids of a checkpoint's pre-image and post-image, so a
    /// caller can compare successive writes of one file without its bytes.
    pub fn images(
        directory: &Path,
        session: SessionId,
        checkpoint: &str,
    ) -> io::Result<(String, String)> {
        let repo = Repository::discover(directory, session)?
            .ok_or_else(|| error("no checkpoints outside Git"))?;
        if !repo.ids()?.iter().any(|id| id == checkpoint) {
            return Err(error("checkpoint does not belong to this session"));
        }
        let blob = |name: &str| {
            text(git(
                &repo.root,
                &["rev-parse", &format!("{checkpoint}:{name}")],
                &[],
            )?)
        };
        Ok((blob("before")?, blob("after")?))
    }

    /// The preview of one checkpoint of the session.
    pub fn preview_one(
        directory: &Path,
        session: SessionId,
        checkpoint: &str,
    ) -> io::Result<Preview> {
        let repo = Repository::discover(directory, session)?
            .ok_or_else(|| error("no checkpoints outside Git"))?;
        let mut lock = repo.lock()?;
        let _guard = lock.write()?;
        repo.preview(checkpoint)
    }

    /// Restore exactly one selected file. Resolve conflicts outside this API, then preview again.
    pub fn restore(directory: &Path, session: SessionId, checkpoint: &str) -> io::Result<Preview> {
        let repo = Repository::discover(directory, session)?
            .ok_or_else(|| error("no checkpoints outside Git"))?;
        let mut lock = repo.lock()?;
        let _guard = lock.write()?;
        let preview = repo.preview(checkpoint)?;
        if preview.conflict {
            return Err(error(
                "external change conflicts with checkpoint; preserve or resolve it explicitly before restoring",
            ));
        }
        if !preview.already_original {
            let (record, before, _) = repo.load(checkpoint)?;
            let path = repo.checked_path(&record.path)?;
            if record.existed {
                let replacement = PreparedReplacement::new(&path, &before)?;
                repo.capture(&path, &before, &replacement, true)?;
                replacement.commit()?;
            } else {
                fs::remove_file(&path)?;
                sync_directory(path.parent().expect("checked parent"))?;
            }
        }
        Ok(preview)
    }

    /// Explicit session close discards reachability, not the user's worktree.
    pub fn close(directory: &Path, session: SessionId) -> io::Result<()> {
        if let Some(repo) = Repository::discover(directory, session)? {
            let mut lock = repo.lock()?;
            let _guard = lock.write()?;
            git(&repo.root, &["update-ref", "-d", &repo.reference], &[])?;
        }
        Ok(())
    }
}

impl PathResolver for CheckpointFs {
    fn canonicalize(&self, path: &Path) -> Result<CanonPath, FsError> {
        OsFs.canonicalize(path)
    }
}

impl Fs for CheckpointFs {
    fn write_attempt(&self, path: &CanonPath, bytes: &[u8]) -> FsWriteAttempt {
        if !self.enabled {
            return FsWriteAttempt {
                result: OsFs.write(path, bytes),
                checkpoint: None,
            };
        }
        let mut checkpoint = None;
        let result = (|| -> io::Result<()> {
            let parent = path
                .as_path()
                .parent()
                .ok_or_else(|| error("missing write parent"))?;
            if let Some(repo) = Repository::discover(parent, self.session)? {
                let mut lock = repo.lock()?;
                let _guard = lock.write()?;
                let replacement = PreparedReplacement::new(path.as_path(), bytes)?;
                checkpoint = Some(repo.capture(path.as_path(), bytes, &replacement, false)?);
                replacement.commit()?;
            } else {
                OsFs.write(path, bytes).map_err(error)?;
            }
            Ok(())
        })()
        .map_err(|e| FsError::Io {
            path: path.clone(),
            message: e.to_string(),
        });
        FsWriteAttempt { result, checkpoint }
    }
    fn read(&self, path: &CanonPath) -> Result<Vec<u8>, FsError> {
        OsFs.read(path)
    }
    fn read_bounded(&self, path: &CanonPath, limit: usize) -> Result<Vec<u8>, FsError> {
        OsFs.read_bounded(path, limit)
    }
    fn scan(&self, request: &ScanRequest) -> Result<Scan, FsError> {
        OsFs.scan(request)
    }
    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), FsError> {
        let attempt = self.write_attempt(path, bytes);
        if attempt.result.is_ok()
            && let Some(checkpoint) = attempt.checkpoint
        {
            self.last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(path.as_path().to_path_buf(), checkpoint);
        }
        attempt.result
    }
    fn last_checkpoint(&self, path: &CanonPath) -> Option<String> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path.as_path())
            .cloned()
    }

    fn write_notice(&self, path: &CanonPath) -> Option<String> {
        if !self.enabled {
            return Some("No checkpoint: checkpoints are off.".into());
        }
        match Repository::discover(path.as_path().parent()?, self.session) {
            Ok(Some(_)) => Some(format!("Git checkpoint saved for session {}", self.session)),
            _ => Some("No checkpoint: this file is outside Git.".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_intent_recovers_crashes_on_either_side_of_atomic_write() {
        // Catches: treating a published preimage as proof that a write completed.
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "--quiet"], &[]).unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let session = SessionId::new();
        let repo = Repository::discover(&root, session).unwrap().unwrap();
        let path = root.join("target");
        fs::write(&path, b"before").unwrap();
        let replacement = PreparedReplacement::new(&path, b"after").unwrap();
        let id = repo.capture(&path, b"after", &replacement, false).unwrap();
        // Crash after update-ref, before write: no mutation is needed.
        assert!(
            CheckpointFs::restore(&root, session, &id)
                .unwrap()
                .already_original
        );
        assert_eq!(fs::read(&path).unwrap(), b"before");
        // Crash after the atomic write: the already durable expected postimage
        // allows a fresh adapter to restore, without an in-memory completion flag.
        replacement.commit().unwrap();
        assert!(
            !CheckpointFs::restore(&root, session, &id)
                .unwrap()
                .already_original
        );
        assert_eq!(fs::read(&path).unwrap(), b"before");
    }
}
