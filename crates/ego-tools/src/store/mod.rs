//! The filesystem session store: a JSONL file per session.
//!
//! See `ARCHITECTURE.md` §5 and `plans/m0a-contracts.md` Step 4. This is
//! where the real I/O behind `ego-core`'s `SessionStore` port lives, because
//! `ego-core` opens no files.

mod io;
mod jsonl_store;
// Public as a module rather than re-exported flat like its siblings, only
// until Step 5 lands: a `pub use lease::{Lease, LeaseError}` against an
// empty module fails the whole crate's build, which blocks every other
// agent working in `ego-tools`. Flattened to match `io`/`jsonl_store` once
// the types exist.
pub mod lease;

pub use io::{OsIo, StoreIo};
pub use jsonl_store::JsonlStore;
