//! The filesystem session store: a JSONL file per session.
//!
//! See `ARCHITECTURE.md` §5 and `plans/m0a-contracts.md` Step 4. This is
//! where the real I/O behind `ego-core`'s `SessionStore` port lives, because
//! `ego-core` opens no files.

mod io;
mod jsonl_store;

pub use io::{OsIo, StoreIo};
pub use jsonl_store::JsonlStore;
