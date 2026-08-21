//! Helpers and fakes shared by more than one integration test in this crate.
//!
//! Lives under `tests/common/` rather than `tests/` so cargo treats it as a
//! module to include, not as a test binary of its own.
//!
//! `#![allow(dead_code)]` because Rust compiles this module separately into
//! every test binary that declares `mod common;`, so anything only one of them
//! uses is dead in the others. That is a property of the test harness, not a
//! sign of unused code.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ego_core::permit::{CanonPath, Fs, PathResolver};

/// Deserialise a fixed UUID literal into any of the id newtypes, via their
/// already-derived (and already-public) `Deserialize` impl. This gets
/// deterministic, golden-comparable ids without adding a constructor to
/// `event::id` (out of scope: owned by another story).
pub fn id<T: serde::de::DeserializeOwned>(uuid: &str) -> T {
    serde_json::from_str(&format!("{uuid:?}")).unwrap()
}

/// An in-memory, test-only `Fs`: a flat table from path to either file
/// bytes or a symlink target, resolved by whole-path substitution (not
/// POSIX component-by-component — this fake only needs to model "what
/// does this path resolve to right now", including changing its answer
/// between two calls, which is exactly the TOCTOU this story guards
/// against).
#[derive(Default)]
pub struct FakeFs {
    table: Mutex<HashMap<PathBuf, Node>>,
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

    /// Puts `bytes` at `path`, so a read tool has something to find.
    pub fn file(&self, path: &str, bytes: &str) {
        self.table
            .lock()
            .unwrap()
            .insert(PathBuf::from(path), Node::File(bytes.as_bytes().to_vec()));
    }
}

impl PathResolver for FakeFs {
    fn canonicalize(&self, path: &Path) -> CanonPath {
        let table = self.table.lock().unwrap();
        let mut current = path.to_path_buf();
        for _ in 0..32 {
            match table.get(&current) {
                Some(Node::Symlink(target)) => current = target.clone(),
                _ => return CanonPath::new(current),
            }
        }
        panic!("FakeFs::canonicalize: symlink loop at {}", path.display());
    }
}

impl Fs for FakeFs {
    fn read(&self, path: &CanonPath) -> Option<Vec<u8>> {
        match self.table.lock().unwrap().get(path.as_path()) {
            Some(Node::File(bytes)) => Some(bytes.clone()),
            _ => None,
        }
    }

    fn write(&self, path: &CanonPath, bytes: &[u8]) {
        self.table
            .lock()
            .unwrap()
            .insert(path.as_path().to_path_buf(), Node::File(bytes.to_vec()));
    }
}
