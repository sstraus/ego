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

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The scratch path [`StoreIo::replace_atomic`] writes before renaming it
/// onto `path`.
///
/// Public, and part of the `StoreIo` contract rather than `OsIo`'s private
/// business, for one reason: crash recovery has to be able to *find* a
/// scratch file left behind by a crash between the write and the rename
/// (`plans/m0a-contracts.md` Step 6's "torn manifest tmp" kill point). A
/// caller cannot clean up a file whose name only the implementation knows,
/// so the name is derived here, once, and every `StoreIo` implementation
/// must use it.
///
/// The suffix is *appended* (`manifest.json` → `manifest.json.tmp`) rather
/// than substituted via `Path::with_extension`, which would replace the
/// existing extension and yield `manifest.tmp` — a name that could collide
/// with a real file and that loses which target it belongs to.
pub fn tmp_path_for(path: &Path) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(".tmp");
    PathBuf::from(name)
}

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

    /// Whether `path` exists. A distinct operation from [`StoreIo::read`] so
    /// a caller that only needs to know "does this log exist yet" is not
    /// forced to pull the whole file into memory to answer a boolean —
    /// `JsonlStore::append` is exactly that caller, and a session's log can
    /// grow to the hundreds of megabytes `ARCHITECTURE.md` §5 expects
    /// snapshots to bound.
    fn exists(&self, path: &Path) -> io::Result<bool>;

    /// Atomically and *durably* replace `path`'s contents with `bytes`: a
    /// reader must never observe a partially written file, and a crash
    /// after this call returns must leave either the old contents or the
    /// new ones — never a truncated file and never a resurrected old
    /// version.
    ///
    /// Implemented as write-to-[`tmp_path_for`] then rename, both behind
    /// this one call. Atomicity alone is not enough: `rename` is atomic with
    /// respect to concurrent *readers*, but says nothing about what survives
    /// a power loss, so an implementation must also fsync the scratch file
    /// before the rename (or the rename can expose a file whose contents
    /// were never flushed) and fsync the parent directory after it (or the
    /// directory entry itself can be lost).
    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;

    /// Shorten the file at `path` to `len` bytes, discarding everything
    /// past it. Used by tail repair to drop a torn final line
    /// (`plans/m0a-contracts.md` Step 6); a whole-file rewrite through
    /// [`StoreIo::replace_atomic`] would be correct but would copy an
    /// entire session log — up to the hundreds of megabytes
    /// `ARCHITECTURE.md` §5 expects — to remove a few trailing bytes.
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()>;

    /// Delete the file at `path`, succeeding if it is already gone. Used to
    /// clear a scratch file orphaned by a crash mid-[`StoreIo::replace_atomic`].
    fn remove_file(&self, path: &Path) -> io::Result<()>;

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

    fn exists(&self, path: &Path) -> io::Result<bool> {
        // `Path::exists()` swallows a permissions error into `false`, which
        // would make an unreadable log look like a brand-new session and
        // re-write a header into a log that already has one. `try_exists()`
        // surfaces that error instead of hiding it.
        path.try_exists()
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp_path = tmp_path_for(path);

        // Write *and fsync* the scratch file before renaming it into place.
        // `fs::write` only guarantees the bytes reached the OS page cache;
        // renaming an unflushed file is what produces the classic
        // "atomically replaced the manifest with zeroes" crash outcome,
        // because the rename can reach stable storage before the contents
        // do.
        {
            use std::io::Write;
            let mut file = fs::File::create(&tmp_path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }

        fs::rename(&tmp_path, path)?;

        // Then fsync the *directory*, which is what actually makes the
        // rename durable: the file's own contents are already on stable
        // storage, but the directory entry pointing at them is a separate
        // write that a crash can still lose.
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        let file = fs::OpenOptions::new()
            .write(true)
            // Declared explicitly: `set_len` below is the only shortening
            // this performs, and opening with `truncate(true)` would
            // discard the whole log before we could keep its valid prefix.
            .truncate(false)
            .open(path)?;
        file.set_len(len)?;
        // A repair that a crash can undo is not a repair: the next open
        // would find the same torn tail. Force the shortened length out
        // before reporting success.
        file.sync_all()
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
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
