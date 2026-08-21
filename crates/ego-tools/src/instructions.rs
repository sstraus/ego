//! Reading `AGENTS.md` off a real filesystem.
//!
//! The `std::fs` half of `ego_core::config`'s instruction discovery; the
//! decision half — which paths, in what order, under what trust — is pure and
//! lives there. See
//! `docs/decisions/2026-08-21-instructions-a-workspace-may-hand-a-model.md`.
//!
//! This is not `OsFs`. That one is the **model-controlled** seam a tool
//! reaches through a permit; this one is trusted-side and runs before a loop,
//! a permit or a tool exists. Keeping them apart is the same rule that keeps
//! `StoreIo` and `Fs` apart: a seam that is a security boundary for one caller
//! and not for another is true right up until someone adds a method.

use std::fs;
use std::io::Read;
use std::path::Path;

use ego_core::config::{Found, InstructionIo, Refusal};

/// [`InstructionIo`] over `std::fs`.
pub struct OsInstructionIo;

impl InstructionIo for OsInstructionIo {
    fn load(&self, path: &Path, max_bytes: u64) -> Found {
        // `symlink_metadata`, not `metadata`: the whole point is to see the
        // link itself rather than what it points at. A symlinked instruction
        // file is refused, not resolved — see `Refusal::Symlink`.
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Found::Absent,
            Err(err) => {
                return Found::Refused(Refusal::Unreadable {
                    message: err.to_string(),
                });
            }
        };

        if meta.file_type().is_symlink() {
            return Found::Refused(Refusal::Symlink);
        }
        if !meta.is_file() {
            return Found::Refused(Refusal::NotAFile);
        }
        // Checked here as well as below, so an oversized file is refused
        // without being opened at all. The read still enforces it, because
        // this answer is stale the moment it is given.
        if meta.len() > max_bytes {
            return Found::Refused(Refusal::TooLarge { limit: max_bytes });
        }

        let file = match fs::File::open(path) {
            Ok(file) => file,
            // The path was there for `symlink_metadata` and is gone now: a
            // race, and "absent" is the honest report of it.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Found::Absent,
            Err(err) => {
                return Found::Refused(Refusal::Unreadable {
                    message: err.to_string(),
                });
            }
        };

        // One byte past the limit, and no further: enough to tell "exactly at
        // the limit" from "over it" without pulling an arbitrarily large file
        // into memory when the metadata above was wrong or raced.
        let mut bytes = Vec::new();
        if let Err(err) = file.take(max_bytes + 1).read_to_end(&mut bytes) {
            return Found::Refused(Refusal::Unreadable {
                message: err.to_string(),
            });
        }
        if bytes.len() as u64 > max_bytes {
            return Found::Refused(Refusal::TooLarge { limit: max_bytes });
        }

        Found::Read { bytes }
    }
}
