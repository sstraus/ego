//! Transparent access to cold session files.
//!
//! A cold `events.jsonl`, `manifest.json` or `evidence/<id>.blob` lives as a
//! zstd sibling `<name>.zst`. The plain file always wins: the compression job
//! publishes the sibling before it deletes the plain file, and a restore
//! publishes the plain file before it deletes the sibling. A reader that
//! misses both looks at the plain file once more, because a restore can
//! complete between its two lookups.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};

use super::io::{OsIo, StoreIo as _};

/// The compressed sibling of `path`: `events.jsonl` becomes `events.jsonl.zst`.
pub(crate) fn packed_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(".zst");
    PathBuf::from(name)
}

/// Only these session files ever have a compressed sibling. Other paths keep
/// plain-file semantics, so a lookup never resurrects an unrelated name.
pub(crate) fn is_compressible(path: &Path) -> bool {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("events.jsonl" | "manifest.json") => true,
        Some(name) => name.ends_with(".blob"),
        None => false,
    }
}

/// Decompress the sibling of `path`, or `None` when it is absent.
fn read_packed(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::File::open(packed_path(path)) {
        Ok(file) => zstd::stream::decode_all(io::BufReader::new(file)).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The content of a compressible file whose plain form was just found absent.
pub(crate) fn read_absent(path: &Path) -> io::Result<Option<Vec<u8>>> {
    if !is_compressible(path) {
        return Ok(None);
    }
    if let Some(bytes) = read_packed(path)? {
        return Ok(Some(bytes));
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Metadata of the plain file, else of its compressed sibling.
pub(crate) fn metadata(path: &Path) -> io::Result<Option<fs::Metadata>> {
    let packed = packed_path(path);
    for candidate in [path, packed.as_path(), path] {
        match fs::metadata(candidate) {
            Ok(metadata) => return Ok(Some(metadata)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// Readable and seekable, whichever form the file has.
pub(crate) trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// A seekable view of the plain file, else of its decompressed sibling.
pub(crate) fn open(path: &Path) -> io::Result<Box<dyn ReadSeek>> {
    match fs::File::open(path) {
        Ok(file) => Ok(Box::new(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => match read_absent(path)? {
            Some(bytes) => Ok(Box::new(io::Cursor::new(bytes))),
            None => Err(error),
        },
        Err(error) => Err(error),
    }
}

/// Return a compressed file to its plain form before a writer changes it.
/// The caller holds the session's `events.lock`, which the job also needs to
/// publish, so the two never interleave. A plain file that already exists
/// is authoritative and only loses its stale sibling.
pub(crate) fn restore_plain(path: &Path) -> io::Result<()> {
    if path.try_exists()? {
        return remove_if_present(&packed_path(path));
    }
    match read_packed(path)? {
        // Temp file, fsync, rename and directory fsync; then the sibling goes.
        Some(bytes) => OsIo.replace_atomic(path, &bytes),
        None => Ok(()),
    }
}

/// [`restore_plain`] for a mutation that is about to open `path`: free when
/// the plain file exists, which is every append of a live session.
pub(crate) fn restore_before_write(path: &Path) -> io::Result<()> {
    if is_compressible(path) && !path.try_exists()? {
        restore_plain(path)?;
    }
    Ok(())
}

pub(crate) fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}
