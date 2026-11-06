//! `StoreIo`: the small filesystem seam `JsonlStore` performs its real I/O
//! through, and `OsIo`, its `std::fs`-backed implementation.
//!
//! Named `StoreIo` rather than `Fs` deliberately: `ego_core::permit::Fs`
//! already names a different seam (permit-checked read/write over
//! canonical paths, ). This one is unchecked — nothing here
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

use super::cold_read;
use crate::privacy::{PrivatePathKind, harden_private_path};

/// Read the first newline-terminated line without visiting the rest of the file.
fn read_first_line(path: &Path) -> io::Result<Option<Vec<u8>>> {
    use io::BufRead as _;

    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return first_line_of(cold_read::read_absent(path)?);
        }
        Err(error) => return Err(error),
    };
    let mut line = Vec::new();
    io::BufReader::new(file).read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Ok(None);
    }
    if line.pop() != Some(b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session log starts with an unterminated line",
        ));
    }
    Ok(Some(line))
}

/// Read the final newline-terminated line without visiting the rest of the file.
///
/// Store logs define an unterminated final line as torn, so this refuses one
/// instead of returning bytes an append could accidentally splice onto.
fn read_last_line(path: &Path) -> io::Result<Option<Vec<u8>>> {
    use io::{Read as _, Seek as _, SeekFrom};

    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return last_line_of(cold_read::read_absent(path)?);
        }
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(None);
    }

    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    if last[0] != b'\n' {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session log ends in an unterminated line",
        ));
    }

    const CHUNK: u64 = 8 * 1024;
    let mut cursor = len - 1;
    let mut reversed_chunks = Vec::new();
    while cursor > 0 {
        let count = cursor.min(CHUNK);
        let start = cursor - count;
        file.seek(SeekFrom::Start(start))?;
        let mut chunk = vec![0; count as usize];
        file.read_exact(&mut chunk)?;
        if let Some(newline) = chunk.iter().rposition(|byte| *byte == b'\n') {
            reversed_chunks.push(chunk[newline + 1..].to_vec());
            let capacity = reversed_chunks.iter().map(Vec::len).sum();
            let mut line = Vec::with_capacity(capacity);
            for chunk in reversed_chunks.into_iter().rev() {
                line.extend_from_slice(&chunk);
            }
            return Ok(Some(line));
        }
        reversed_chunks.push(chunk);
        cursor = start;
    }

    let capacity = reversed_chunks.iter().map(Vec::len).sum();
    let mut line = Vec::with_capacity(capacity);
    for chunk in reversed_chunks.into_iter().rev() {
        line.extend_from_slice(&chunk);
    }
    Ok(Some(line))
}

/// The first complete line of a whole-file read.
fn first_line_of(bytes: Option<Vec<u8>>) -> io::Result<Option<Vec<u8>>> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
        if bytes.is_empty() {
            return Ok(None);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session log starts with an unterminated line",
        ));
    };
    Ok(Some(bytes[..newline].to_vec()))
}

/// The final complete line of a whole-file read.
fn last_line_of(bytes: Option<Vec<u8>>) -> io::Result<Option<Vec<u8>>> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    if !bytes.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session log ends in an unterminated line",
        ));
    }
    let content = &bytes[..bytes.len() - 1];
    let start = content
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1);
    Ok(Some(content[start..].to_vec()))
}

/// The bytes of a whole-file read from `offset` on.
fn suffix_of(bytes: Option<Vec<u8>>, offset: u64) -> Option<Vec<u8>> {
    bytes.map(|bytes| bytes.get(offset as usize..).unwrap_or_default().to_vec())
}

pub(super) fn open_directory(path: &Path) -> io::Result<fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;

        // Win32 requires FILE_FLAG_BACKUP_SEMANTICS when CreateFile opens a
        // directory handle. The standard File::open path omits it and returns
        // ERROR_ACCESS_DENIED before the durability barrier can run.
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }

    #[cfg(not(windows))]
    fs::File::open(path)
}

fn open_for_sync(path: &Path) -> io::Result<fs::File> {
    #[cfg(windows)]
    {
        fs::OpenOptions::new().read(true).write(true).open(path)
    }

    #[cfg(not(windows))]
    fs::File::open(path)
}

/// The scratch path [`StoreIo::replace_atomic`] writes before renaming it
/// onto `path`.
///
/// A caller cannot clean up a file whose name only the implementation knows, so the name
/// is derived here, once, and every `StoreIo` implementation must use it.
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

/// Whether an atomic replacement was published with confirmed durability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtomicReplacePublication {
    /// Rename published the replacement and the parent directory was synced.
    PublishedDurably,
    /// Rename published the replacement, but the parent-directory durability
    /// operation failed. The diagnostic is the unhidden OS failure.
    PublishedDurabilityUncertain { diagnostic: String },
}

/// One private diagnostic owner; implementations retain an exclusively created handle.
pub trait PrivateLogWriter: Send {
    /// Write a bounded record, or discard it once this owner's cap is exhausted.
    fn append(&mut self, line: &[u8]) -> io::Result<()>;
}

/// A session append descriptor owned by a writer lease. Writes are immediately
/// visible to readers; durability barriers use this same descriptor.
pub trait SessionLogWriter: Send {
    /// Inspect this descriptor after a known append, without resolving its path.
    fn file_revision(&self) -> io::Result<Option<FileRevision>> {
        Ok(None)
    }
    fn append_line(&mut self, line: &[u8]) -> io::Result<()>;
    fn fsync(&self) -> io::Result<()>;
}

impl SessionLogWriter for fs::File {
    fn file_revision(&self) -> io::Result<Option<FileRevision>> {
        FileRevision::from_metadata(&self.metadata()?).map(Some)
    }

    fn append_line(&mut self, line: &[u8]) -> io::Result<()> {
        use io::Write as _;
        self.write_all(line)?;
        self.write_all(b"\n")
    }

    fn fsync(&self) -> io::Result<()> {
        self.sync_all()
    }
}

/// An in-memory file invalidation hint, never a durable content identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRevision {
    pub(crate) modified: std::time::SystemTime,
    created: Option<std::time::SystemTime>,
    pub(crate) length: u64,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl FileRevision {
    pub(crate) fn from_metadata(metadata: &fs::Metadata) -> io::Result<Self> {
        Ok(Self {
            modified: metadata.modified()?,
            created: metadata.created().ok(),
            length: metadata.len(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt as _;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            },
        })
    }
}

/// The filesystem operations the durable session adapters need, and nothing else.
pub trait StoreIo: Send + Sync {
    /// Inspect an invalidation hint without opening the file's content.
    fn file_revision(&self, _path: &Path) -> io::Result<Option<FileRevision>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file revisions unavailable",
        ))
    }

    /// Read an appended suffix. Adapters without ranged reads retain the safe fallback.
    fn read_from(&self, path: &Path, offset: u64) -> io::Result<Option<Vec<u8>>> {
        self.read(path).map(|bytes| suffix_of(bytes, offset))
    }

    /// Return a cold (compressed) session file to its plain form before a
    /// writer changes it. Adapters that never store compressed files keep
    /// this no-op.
    fn restore_plain(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    /// Retain an append descriptor when the adapter supports it. The parent
    /// directory already exists. Other adapters keep their observable I/O seam.
    fn open_session_log(&self, _path: &Path) -> io::Result<Option<Box<dyn SessionLogWriter>>> {
        Ok(None)
    }

    /// Exclusively create one retained owner file; never reopen an existing log.
    fn open_private_log(
        &self,
        _root: &Path,
        _max_bytes: u64,
    ) -> io::Result<Box<dyn PrivateLogWriter>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private log creation unavailable",
        ))
    }

    /// Atomically publish a private advisory document. Unsupported adapters
    /// report a cache miss; they must not silently publish with wider permissions.
    fn replace_atomic_private(&self, _path: &Path, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private atomic publication unavailable",
        ))
    }
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

    /// Force buffered writes for `path` to stable storage as an observable operation,
    /// separate from append.
    fn fsync(&self, path: &Path) -> io::Result<()>;

    /// Read the whole file at `path`, or `None` if it does not exist.
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>>;

    /// Read only the first complete line at `path`, or `None` when the file is
    /// absent or empty. The default delegates exclusively to [`StoreIo::read`]
    /// so an adapter remains the sole owner of the path it represents.
    fn read_first_line(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        first_line_of(self.read(path)?)
    }

    /// Read only the final complete line at `path`, or `None` when the file
    /// is absent or empty.
    ///
    /// The default delegates exclusively to [`StoreIo::read`] so an adapter
    /// remains the sole owner of the path it represents. Ordinary production
    /// appends use the efficient [`OsIo`] override to derive the next physical
    /// record position without an O(log size) whole-file read.
    fn read_last_line(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        last_line_of(self.read(path)?)
    }

    /// Test path existence without reading whole files into memory.
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

    /// Replace `path` while distinguishing errors before publication from a
    /// failed durability operation after the rename made new bytes visible.
    ///
    /// Adapters must implement this only when they can prove that phase
    /// boundary. The default fails without performing I/O, so a generic fake
    /// cannot accidentally claim durable publication by forwarding the older
    /// phase-erasing method.
    fn replace_atomic_publication(
        &self,
        _path: &Path,
        _bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "phase-aware atomic replacement is not implemented",
        ))
    }

    /// Shorten a file to `len` for torn-tail repair without copying the entire log.
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
/// The only thing that differs in the legacy filesystem operations shared by
/// [`OsIo`] and [`NonDurableOsIo`]. Every such operation below is written once
/// and takes this as a parameter, so the two types cannot drift into different
/// rename/truncate semantics. The phase-aware publication method is the one
/// deliberate exception: `NonDurableOsIo` leaves its default unsupported,
/// because an adapter that elides durability barriers cannot report one as
/// confirmed.
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

/// The internal publication result retains the original I/O error so legacy
/// callers keep its kind while the phase-aware port receives its diagnostic.
enum ReplacePublication {
    PublishedDurably,
    PublishedDurabilityUncertain(io::Error),
}

/// Atomic replacement with the rename boundary represented explicitly.
fn replace_atomic_publication_with(
    path: &Path,
    bytes: &[u8],
    barrier: Barrier,
) -> io::Result<ReplacePublication> {
    replace_atomic_publication_with_privacy(path, bytes, barrier, false)
}

fn replace_atomic_publication_with_privacy(
    path: &Path,
    bytes: &[u8],
    barrier: Barrier,
    private: bool,
) -> io::Result<ReplacePublication> {
    let tmp_path = tmp_path_for(path);

    // Write *and fsync* the scratch file before renaming it into place.
    // `fs::write` only guarantees the bytes reached the OS page cache;
    // renaming an unflushed file is what produces the classic
    // "atomically replaced the manifest with zeroes" crash outcome,
    // because the rename can reach stable storage before the contents
    // do.
    {
        use std::io::Write;
        let open = || {
            if private {
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                options.open(&tmp_path)
            } else {
                fs::File::create(&tmp_path)
            }
        };
        let mut file = match open() {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                open()?
            }
            Err(error) => return Err(error),
        };
        if private {
            harden_private_path(&tmp_path, PrivatePathKind::File)?;
        }
        if let Err(error) = file.write_all(bytes).and_then(|()| barrier.apply(&file)) {
            drop(file);
            if private {
                let _ = fs::remove_file(&tmp_path);
            }
            return Err(error);
        }
    }

    if let Err(error) = fs::rename(&tmp_path, path) {
        if private {
            let _ = fs::remove_file(&tmp_path);
        }
        return Err(error);
    }

    // Then fsync the *directory*, which is what actually makes the
    // rename durable: the file's own contents are already on stable
    // storage, but the directory entry pointing at them is a separate
    // write that a crash can still lose.
    if let Some(parent) = path.parent() {
        let directory = match open_directory(parent) {
            Ok(directory) => directory,
            Err(error) => return Ok(ReplacePublication::PublishedDurabilityUncertain(error)),
        };
        if let Err(error) = barrier.apply(&directory) {
            return Ok(ReplacePublication::PublishedDurabilityUncertain(error));
        }
    }
    // The new plain file is authoritative. A sibling left by a failed unlink
    // is harmless (plain wins), so this cannot turn publication into failure.
    if cold_read::is_compressible(path) {
        let _ = cold_read::remove_if_present(&cold_read::packed_path(path));
    }
    Ok(ReplacePublication::PublishedDurably)
}

/// Preserve the original phase-erasing contract for every existing caller.
fn replace_atomic_with(path: &Path, bytes: &[u8], barrier: Barrier) -> io::Result<()> {
    match replace_atomic_publication_with(path, bytes, barrier)? {
        ReplacePublication::PublishedDurably => Ok(()),
        ReplacePublication::PublishedDurabilityUncertain(error) => Err(error),
    }
}

/// [`StoreIo::truncate`], with `barrier` deciding whether the shortened
/// length is forced out before returning.
fn truncate_with(path: &Path, len: u64, barrier: Barrier) -> io::Result<()> {
    cold_read::restore_before_write(path)?;
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
    const CHUNK: usize = 32 * 1024;
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let Some(bytes) = cold_read::read_absent(path)? else {
                return Ok(false);
            };
            for chunk in bytes.chunks(CHUNK) {
                visitor(chunk)?;
            }
            return Ok(true);
        }
        Err(error) => return Err(error),
    };
    let mut chunk = [0_u8; CHUNK];
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
                barrier
                    .apply(&open_for_sync(target)?)
                    .map_err(|error| phase_error("sync published blob", target, error))?;
                if let Some(parent) = target.parent() {
                    barrier
                        .apply(&open_directory(parent)?)
                        .map_err(|error| phase_error("sync blob directory", parent, error))?;
                }
                Ok(())
            })();
            if let Err(error) = durable {
                let _ = fs::remove_file(target);
                return Err(error);
            }
            fs::remove_file(staging)
                .map_err(|error| phase_error("remove blob staging file", staging, error))?;
            if let Some(parent) = staging.parent()
                && let Err(error) = barrier
                    .apply(&open_directory(parent)?)
                    .map_err(|error| phase_error("sync staging directory", parent, error))
            {
                let _ = fs::remove_file(target);
                if let Ok(directory) = open_directory(parent) {
                    let _ = barrier.apply(&directory);
                }
                return Err(error);
            }
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(phase_error("publish blob hard link", target, error)),
    }
}

fn phase_error(action: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{action} at {}: {error}", path.display()),
    )
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
    fn file_revision(&self, path: &Path) -> io::Result<Option<FileRevision>> {
        let metadata = if cold_read::is_compressible(path) {
            cold_read::metadata(path)
        } else {
            match fs::metadata(path) {
                Ok(metadata) => Ok(Some(metadata)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        };
        metadata?
            .map(|metadata| FileRevision::from_metadata(&metadata))
            .transpose()
    }

    fn read_from(&self, path: &Path, offset: u64) -> io::Result<Option<Vec<u8>>> {
        use io::{Read as _, Seek as _, SeekFrom};
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(suffix_of(cold_read::read_absent(path)?, offset));
            }
            Err(error) => return Err(error),
        };
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    fn open_session_log(&self, path: &Path) -> io::Result<Option<Box<dyn SessionLogWriter>>> {
        cold_read::restore_before_write(path)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Some(Box::new(file)))
    }

    fn open_private_log(
        &self,
        root: &Path,
        max_bytes: u64,
    ) -> io::Result<Box<dyn PrivateLogWriter>> {
        super::private_log::open(root, max_bytes)
    }

    fn replace_atomic_private(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        match replace_atomic_publication_with_privacy(path, bytes, Barrier::Fsync, true)? {
            ReplacePublication::PublishedDurably => Ok(()),
            ReplacePublication::PublishedDurabilityUncertain(error) => Err(error),
        }
    }
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
        cold_read::restore_before_write(path)?;
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
        // Reopen without truncation after the append handle has flushed.
        // Unix can sync a read handle; Windows FlushFileBuffers requires
        // write access even though this operation does not modify bytes.
        if path.is_dir() {
            open_directory(path)?.sync_all()
        } else {
            open_for_sync(path)?.sync_all()
        }
    }

    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => cold_read::read_absent(path),
            Err(e) => Err(e),
        }
    }

    fn restore_plain(&self, path: &Path) -> io::Result<()> {
        cold_read::restore_plain(path)
    }

    fn read_first_line(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        read_first_line(path)
    }

    fn read_last_line(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        read_last_line(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        // `Path::exists()` swallows a permissions error into `false`, which
        // would make an unreadable log look like a brand-new session and
        // re-write a header into a log that already has one. `try_exists()`
        // surfaces that error instead of hiding it.
        Ok(path.try_exists()?
            || (cold_read::is_compressible(path) && cold_read::packed_path(path).try_exists()?))
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        replace_atomic_with(path, bytes, Barrier::Fsync)
    }

    fn replace_atomic_publication(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        match replace_atomic_publication_with(path, bytes, Barrier::Fsync)? {
            ReplacePublication::PublishedDurably => Ok(AtomicReplacePublication::PublishedDurably),
            ReplacePublication::PublishedDurabilityUncertain(error) => {
                Ok(AtomicReplacePublication::PublishedDurabilityUncertain {
                    diagnostic: error.to_string(),
                })
            }
        }
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        truncate_with(path, len, Barrier::Fsync)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        // A compressed sibling left behind would resurrect the removed file.
        if cold_read::is_compressible(path) {
            cold_read::remove_if_present(&cold_read::packed_path(path))?;
        }
        cold_read::remove_if_present(path)
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
/// Every legacy operation is the real one: the same `create_dir_all`, the
/// same tmp-write-then-`rename`, the same `set_len`, the same `read_dir` — see
/// `replace_atomic_with` and `truncate_with`, which both types share so there
/// is exactly one implementation of these semantics. Only `Barrier::Fsync`
/// becomes `Barrier::Elided`. A test running on this still meets real rename
/// and truncate behaviour, and so still catches the class of bug a hand-written
/// in-memory fake would stop catching. It intentionally does not override
/// [`StoreIo::replace_atomic_publication`]: no elided barrier may claim durable
/// publication.
///
/// **What is given up, precisely:** a crash after one of these calls returns
/// may leave the old contents, or a lost directory entry, or a torn tail
/// this type reported as repaired. Within a single live process nothing
/// changes at all — the writes reach the OS page cache either way, so every
/// read-back sees identical bytes. That is the whole reason it is safe for a
/// test that reads back in-process, and the whole reason it is wrong
/// anywhere else.
///
/// Test-only I/O elides durability barriers but retains actual filesystem operations. It
/// must never claim crash durability or enable durable publication without barriers.
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

    fn restore_plain(&self, path: &Path) -> io::Result<()> {
        OsIo.restore_plain(path)
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
