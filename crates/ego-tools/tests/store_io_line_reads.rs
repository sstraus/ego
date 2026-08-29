//! RED contracts for adapter-owned default `StoreIo` line reads.

use ego_tools::store::StoreIo;
use std::io;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

enum ReadMode {
    Bytes(Vec<u8>),
    Error,
}

struct Adapter {
    mode: ReadMode,
}

impl StoreIo for Adapter {
    fn append_line(&self, _path: &Path, _line: &[u8]) -> io::Result<()> {
        Err(io::Error::other("not used by line-read fixture"))
    }

    fn fsync(&self, _path: &Path) -> io::Result<()> {
        Err(io::Error::other("not used by line-read fixture"))
    }

    fn read(&self, _path: &Path) -> io::Result<Option<Vec<u8>>> {
        match &self.mode {
            ReadMode::Bytes(bytes) => Ok(Some(bytes.clone())),
            ReadMode::Error => Err(io::Error::other("injected adapter read failure")),
        }
    }

    fn exists(&self, _path: &Path) -> io::Result<bool> {
        Ok(false)
    }

    fn replace_atomic(&self, _path: &Path, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::other("not used by line-read fixture"))
    }

    fn truncate(&self, _path: &Path, _len: u64) -> io::Result<()> {
        Err(io::Error::other("not used by line-read fixture"))
    }

    fn remove_file(&self, _path: &Path) -> io::Result<()> {
        Err(io::Error::other("not used by line-read fixture"))
    }

    fn list_dirs(&self, _path: &Path) -> io::Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }
}

#[test]
fn default_line_reads_use_adapter_bytes_not_an_unrelated_real_file() {
    let root = tempdir().expect("temporary line-read root");
    let path = root.path().join("shadow.log");
    std::fs::write(&path, b"real-first\nreal-last\n").expect("write unrelated shadow file");
    let adapter = Adapter {
        mode: ReadMode::Bytes(b"adapter-first\nadapter-last\n".to_vec()),
    };

    let first = adapter
        .read_first_line(&path)
        .expect("adapter first-line read");
    let last = adapter
        .read_last_line(&path)
        .expect("adapter last-line read");

    assert_eq!(first.as_deref(), Some(b"adapter-first".as_slice()));
    assert_eq!(last.as_deref(), Some(b"adapter-last".as_slice()));
}

#[test]
fn default_line_reads_return_injected_adapter_errors_even_when_a_shadow_file_exists() {
    let root = tempdir().expect("temporary error root");
    let path = root.path().join("shadow.log");
    std::fs::write(&path, b"real-first\nreal-last\n").expect("write unrelated shadow file");
    let adapter = Adapter {
        mode: ReadMode::Error,
    };

    assert!(adapter.read_first_line(&path).is_err());
    assert!(adapter.read_last_line(&path).is_err());
}
