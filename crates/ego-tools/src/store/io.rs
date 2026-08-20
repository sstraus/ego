//! `StoreIo`: the small filesystem seam `JsonlStore` performs its real I/O
//! through, and `OsIo`, its `std::fs`-backed implementation.
//!
//! Named `StoreIo` rather than `Fs` deliberately: `ego_core::permit::Fs`
//! already names a different seam (permit-checked read/write over
//! canonical paths, regression case). This one is unchecked — nothing here
//! consults a `Permit` — and it is shaped around session-store durability,
//! not tool sandboxing: fsync is its own operation, distinct from writing,
//! so a caller (or a test) can observe exactly when durability was
//! requested independent of how many lines were appended; and atomic
//! replacement is a single operation, not "write tmp" plus "rename" left
//! for every call site to compose correctly — the tmp+rename invariant
//! lives here, once, so a caller cannot forget the rename half.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The filesystem operations `JsonlStore` needs, and nothing else.
pub trait StoreIo: Send + Sync {
    /// Append `line` (no trailing newline) plus a newline to the file at
    /// `path`, creating the file and any missing parent directories if
    /// `path` does not exist yet.
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()>;

    /// Force `path`'s buffered writes to stable storage. Kept distinct from
    /// [`StoreIo::append_line`] so the fsync policy (`ARCHITECTURE.md` §5)
    /// is observable as its own event, not inferred from write counts.
    fn fsync(&self, path: &Path) -> io::Result<()>;

    /// Read the whole file at `path`, or `None` if it does not exist.
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>>;

    /// Atomically replace `path`'s contents with `bytes`: a reader must
    /// never observe a partially written file. Implemented as write-to-tmp
    /// then rename, both behind this one call.
    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;

    /// List the immediate child directories of `path`, or an empty list if
    /// `path` does not exist.
    fn list_dirs(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
}

/// The real, `std::fs`-backed [`StoreIo`].
#[derive(Debug, Default, Clone, Copy)]
pub struct OsIo;

impl StoreIo for OsIo {
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(line)?;
        file.write_all(b"\n")?;
        Ok(())
    }

    fn fsync(&self, path: &Path) -> io::Result<()> {
        // Opened for read: syncing an already-flushed append handle needs
        // no write access, and this avoids truncating or otherwise
        // disturbing the file just to sync it.
        fs::File::open(path)?.sync_all()
    }

    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, bytes)?;
        fs::rename(&tmp_path, path)?;
        Ok(())
    }

    fn list_dirs(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut dirs = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                dirs.push(entry.path());
            }
        }
        Ok(dirs)
    }
}
