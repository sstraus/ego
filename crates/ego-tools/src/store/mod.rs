//! The filesystem session store: a JSONL file per session.
//!
//! See `ARCHITECTURE.md` §5 and `plans/m0a-contracts.md` Step 4. This is
//! where the real I/O behind `ego-core`'s `SessionStore` port lives, because
//! `ego-core` opens no files.

mod blob;
mod catalogue;
mod cold;
mod cold_read;
mod evidence;
mod history;
mod io;
mod jsonl_store;
mod lease;
mod private_log;
mod retention;
mod run_artifact;
mod snapshot;
mod writer;

pub use blob::SessionBlobStore;
pub use cold::{
    ColdStoragePolicy, ColdStorageReport, JOB_NICE_INCREMENT, lower_job_thread_priority,
};
pub use evidence::SessionEvidenceStore;
pub use history::{HistoryError, HistoryHit, HistoryIndex};
#[cfg(feature = "test-support")]
pub use io::NonDurableOsIo;
pub use io::{
    AtomicReplacePublication, FileRevision, OsIo, PrivateLogWriter, SessionLogWriter, StoreIo,
    tmp_path_for,
};
pub use jsonl_store::{JsonlStore, Repair, ResumeListing};
pub use lease::{Lease, LeaseError};
pub use retention::RetentionReport;
pub use snapshot::{SNAPSHOT_VERSION, Snapshot, SnapshotPolicy, SnapshotRejection, validate};
pub use writer::{LeaseInputs, OpenError, OpenSession, SessionWriter};

/// Return the complete JSONL records, including the final newline but not a torn tail.
pub(crate) fn committed_jsonl_prefix(bytes: &[u8]) -> &[u8] {
    bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(&[][..], |last| &bytes[..=last])
}
