//! The filesystem session store: a JSONL file per session.
//!
//! See `ARCHITECTURE.md` §5 and `plans/m0a-contracts.md` Step 4. This is
//! where the real I/O behind `ego-core`'s `SessionStore` port lives, because
//! `ego-core` opens no files.

mod io;
mod jsonl_store;
mod lease;
mod snapshot;
mod writer;

#[cfg(feature = "test-support")]
pub use io::NonDurableOsIo;
pub use io::{OsIo, StoreIo, tmp_path_for};
pub use jsonl_store::{JsonlStore, Repair};
pub use lease::{Lease, LeaseError};
pub use snapshot::{SNAPSHOT_VERSION, Snapshot, SnapshotPolicy, SnapshotRejection, validate};
pub use writer::{LeaseInputs, OpenError, OpenSession, SessionWriter};
