// Shared test utilities; each integration binary uses only its relevant subset.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub fn files(root: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("owner-") && name.ends_with(".jsonl"))
        })
        .collect();
    paths.sort();
    paths
}

pub fn bytes(root: &Path) -> Vec<u8> {
    files(root)
        .iter()
        .flat_map(|path| std::fs::read(path).expect("owner log"))
        .collect()
}

pub struct EchoingFailure;
impl ego_tools::store::PrivateLogWriter for EchoingFailure {
    fn append(&mut self, line: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other(
            String::from_utf8_lossy(line).into_owned(),
        ))
    }
}
