//! Read-only inventory for the explicitly authorized schema-5 quarantine.
//! Run by path after a native build; stdout contains private diagnostic metadata.
use std::path::Path;
use std::time::UNIX_EPOCH;

use ego_core::event::id::{Seq, SessionId};
use ego_core::session::ReplaySource;
use ego_core::store::codec::{StoreLine, decode_line};
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{JsonlStore, OsIo};
use futures::{StreamExt, executor::block_on};
use serde_json::json;
use sha2::{Digest, Sha256};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args()
        .nth(1)
        .ok_or("expected session-store path")?;
    let root = Path::new(&root);
    let store = JsonlStore::new(root, "/", Box::new(OsIo));
    let mut paths = std::fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    paths.sort_by_key(|entry| entry.file_name());
    let mut inventory = Vec::new();
    for entry in paths {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_dir() {
            inventory.push(json!({"id": name, "status": "not_directory"}));
            continue;
        }
        let Ok(id) = name.parse::<SessionId>() else {
            inventory.push(json!({"id": name, "status": "not_session_id"}));
            continue;
        };
        let log = entry.path().join("events.jsonl");
        let bytes = match std::fs::read(&log) {
            Ok(bytes) => bytes,
            Err(error) => {
                inventory.push(
                    json!({"id": name, "status": "other_failure", "error": error.to_string()}),
                );
                continue;
            }
        };
        let metadata = std::fs::metadata(&log)?;
        let modified = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)?
            .as_nanos()
            .to_string();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        let mut other_errors = Vec::new();
        let mut schema_errors = Vec::new();
        // The streaming reader visits every entry, unlike replay, which stops
        // at the first rejected entry. Never classify from the first error alone.
        match SessionStore::read(&store, &id) {
            Err(error) => other_errors.push(error.to_string()),
            Ok(stream) => {
                for (index, result) in block_on(stream.collect::<Vec<_>>()).into_iter().enumerate()
                {
                    if let Err(error) = result {
                        let reason = error.to_string();
                        if reason.contains("an old, unsupported capabilities schema 5; this build requires schema 6") {
                            schema_errors.push(json!({"record": index + 1, "error": reason}));
                        } else {
                            other_errors.push(format!("record {}: {reason}", index + 1));
                        }
                    }
                }
            }
        }
        // Replay also checks physical order and complete tails. Check those
        // across the entire log, including records after rejected admissions.
        if bytes.last() != Some(&b'\n') {
            other_errors.push("incomplete final line".to_owned());
        }
        let mut position = Seq::ZERO;
        for (index, line) in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .enumerate()
        {
            match decode_line(line) {
                Ok(StoreLine::Header(header)) if index == 0 && header.session == id => {}
                Ok(StoreLine::Record(record)) if index > 0 && record.seq == position.next() => {
                    position = record.seq;
                }
                _ => other_errors.push(format!(
                    "invalid header, checksum, shape or position at line {}",
                    index + 1
                )),
            }
        }
        let replay_error = ReplaySource::entries(&store, &id)
            .err()
            .map(|error| error.to_string());
        let status = if other_errors.is_empty() && !schema_errors.is_empty() {
            if replay_error.as_ref().is_some_and(|reason| {
                reason.contains(
                    "an old, unsupported capabilities schema 5; this build requires schema 6",
                )
            }) {
                "schema5_only"
            } else {
                "other_failure"
            }
        } else if other_errors.is_empty() && replay_error.is_none() {
            "readable"
        } else {
            "other_failure"
        };
        inventory.push(json!({"id": name, "status": status, "mtime_ns": modified,
            "bytes": bytes.len(), "sha256": hash, "schema_errors": schema_errors,
            "other_errors": other_errors, "replay_error": replay_error}));
    }
    serde_json::to_writer_pretty(std::io::stdout().lock(), &inventory)?;
    Ok(())
}
