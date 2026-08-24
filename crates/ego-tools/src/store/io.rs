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

/// The filesystem operations the durable session adapters need, and nothing else.
pub trait StoreIo: Send + Sync {
    /// Create a new private staging file, refusing an existing name.
    fn create_new(&self, _path: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "incremental evidence creation is not implemented",
        ))
    }

    /// Append arbitrary bytes exactly, without record framing.
    fn append_bytes(&self, _path: &Path, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "incremental evidence append is not implemented",
        ))
    }

    /// Visit a file in bounded chunks, or report that it is absent.
    fn visit_file(
        &self,
        _path: &Path,
        _visitor: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "incremental evidence read is not implemented",
        ))
    }

    /// Publish a staging inode without replacing an existing immutable blob.
    /// Returns `false` when the destination already exists.
    fn publish_new(&self, _staging: &Path, _target: &Path) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "incremental evidence publish is not implemented",
        ))
    }

    /// List immediate regular files, or an empty list for an absent path.
    fn list_files(&self, _path: &Path) -> io::Result<Vec<PathBuf>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "incremental evidence listing is not implemented",
        ))
    }

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

/// Whether a durability barrier actually reaches the device.
///
/// The *only* thing that differs between [`OsIo`] and [`NonDurableOsIo`].
/// Every filesystem operation below is written once and takes this as a
/// parameter, so the two types cannot drift into two different sets of
/// rename/truncate semantics — which is the one way a barrier-free variant
/// could quietly stop testing what the real one does.
/// Without `test-support` this enum has exactly one variant, so eliding a
/// barrier is not merely discouraged in a production build — it cannot be
/// spelled.
#[derive(Debug, Clone, Copy)]
enum Barrier {
    /// `fsync(2)` — on macOS, `F_FULLFSYNC`: a full device barrier.
    Fsync,
    /// Not issued. The write still reaches the OS page cache, so every
    /// reader in this process sees exactly what it would have seen; only
    /// survival across a power loss is given up.
    #[cfg(feature = "test-support")]
    Elided,
}

impl Barrier {
    fn apply(self, file: &fs::File) -> io::Result<()> {
        match self {
            Self::Fsync => file.sync_all(),
            #[cfg(feature = "test-support")]
            Self::Elided => Ok(()),
        }
    }
}

/// [`StoreIo::replace_atomic`], with `barrier` deciding whether the two
/// fsyncs it describes are issued. The tmp+rename half is unconditional.
fn replace_atomic_with(path: &Path, bytes: &[u8], barrier: Barrier) -> io::Result<()> {
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
        barrier.apply(&file)?;
    }

    fs::rename(&tmp_path, path)?;

    // Then fsync the *directory*, which is what actually makes the
    // rename durable: the file's own contents are already on stable
    // storage, but the directory entry pointing at them is a separate
    // write that a crash can still lose.
    if let Some(parent) = path.parent() {
        barrier.apply(&fs::File::open(parent)?)?;
    }
    Ok(())
}

/// [`StoreIo::truncate`], with `barrier` deciding whether the shortened
/// length is forced out before returning.
fn truncate_with(path: &Path, len: u64, barrier: Barrier) -> io::Result<()> {
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
    barrier.apply(&file)
}

fn create_new(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    Ok(())
}

fn append_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use io::Write as _;
    fs::OpenOptions::new()
        .append(true)
        .open(path)?
        .write_all(bytes)
}

fn visit_file(path: &Path, visitor: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<bool> {
    use io::Read as _;
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut chunk = [0_u8; 32 * 1024];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            return Ok(true);
        }
        visitor(&chunk[..read])?;
    }
}

fn publish_new_with(staging: &Path, target: &Path, barrier: Barrier) -> io::Result<bool> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::hard_link(staging, target) {
        Ok(()) => {
            let durable: io::Result<()> = (|| {
                barrier.apply(&fs::File::open(target)?)?;
                if let Some(parent) = target.parent() {
                    barrier.apply(&fs::File::open(parent)?)?;
                }
                Ok(())
            })();
            if let Err(error) = durable {
                let _ = fs::remove_file(target);
                return Err(error);
            }
            fs::remove_file(staging)?;
            if let Some(parent) = staging.parent() {
                if let Err(error) = barrier.apply(&fs::File::open(parent)?) {
                    let _ = fs::remove_file(target);
                    if let Ok(directory) = fs::File::open(parent) {
                        let _ = barrier.apply(&directory);
                    }
                    return Err(error);
                }
            }
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

fn list_files(path: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            files.push(entry.path());
        }
    }
    Ok(files)
}

/// The real, `std::fs`-backed [`StoreIo`]. The only one anything outside a
/// test should ever construct.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsIo;

impl StoreIo for OsIo {
    fn create_new(&self, path: &Path) -> io::Result<()> {
        create_new(path)
    }

    fn append_bytes(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        append_bytes(path, bytes)
    }

    fn visit_file(
        &self,
        path: &Path,
        visitor: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<bool> {
        visit_file(path, visitor)
    }

    fn publish_new(&self, staging: &Path, target: &Path) -> io::Result<bool> {
        publish_new_with(staging, target, Barrier::Fsync)
    }

    fn list_files(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        list_files(path)
    }

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
        replace_atomic_with(path, bytes, Barrier::Fsync)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        truncate_with(path, len, Barrier::Fsync)
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

/// [`OsIo`] with the durability barriers elided, and **nothing else
/// changed**. For tests that sweep a large number of filesystem states and
/// assert nothing about surviving a power loss.
///
/// Every operation is the real one: the same `create_dir_all`, the same
/// tmp-write-then-`rename`, the same `set_len`, the same `read_dir` — see
/// `replace_atomic_with` and `truncate_with`, which both types share so
/// there is exactly one implementation of these semantics. Only
/// `Barrier::Fsync` becomes `Barrier::Elided`. A test running on this still
/// meets real rename and truncate behaviour, and so still catches the class
/// of bug a hand-written in-memory fake would stop catching.
///
/// **What is given up, precisely:** a crash after one of these calls returns
/// may leave the old contents, or a lost directory entry, or a torn tail
/// this type reported as repaired. Within a single live process nothing
/// changes at all — the writes reach the OS page cache either way, so every
/// read-back sees identical bytes. That is the whole reason it is safe for a
/// test that reads back in-process, and the whole reason it is wrong
/// anywhere else.
///
/// `JsonlStore`'s crash-recovery contract (`ARCHITECTURE.md` §5) is a
/// durability claim, and this type does not honour it — which is why it sits
/// behind the `test-support` feature rather than merely carrying this
/// warning. A doc comment is advice; a feature gate is a guarantee. No
/// non-test build enables it, so outside a test this type does not exist to
/// be misused. See the feature's own comment in `crates/ego-tools/Cargo.toml`.
#[cfg(feature = "test-support")]
#[derive(Debug, Default, Clone, Copy)]
pub struct NonDurableOsIo;

#[cfg(feature = "test-support")]
impl StoreIo for NonDurableOsIo {
    fn create_new(&self, path: &Path) -> io::Result<()> {
        create_new(path)
    }

    fn append_bytes(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        append_bytes(path, bytes)
    }

    fn visit_file(
        &self,
        path: &Path,
        visitor: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<bool> {
        visit_file(path, visitor)
    }

    fn publish_new(&self, staging: &Path, target: &Path) -> io::Result<bool> {
        publish_new_with(staging, target, Barrier::Elided)
    }

    fn list_files(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        list_files(path)
    }

    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        OsIo.append_line(path, line)
    }

    /// The barrier itself, so eliding it is the entire point of this type.
    fn fsync(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        OsIo.read(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        OsIo.exists(path)
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        replace_atomic_with(path, bytes, Barrier::Elided)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        truncate_with(path, len, Barrier::Elided)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        OsIo.remove_file(path)
    }

    fn list_dirs(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        OsIo.list_dirs(path)
    }
}
