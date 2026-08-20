//! `SessionState` and the pure, sync reducer.
//!
//! See `ARCHITECTURE.md` §1: state is a reduction over an append-only event
//! log, and compaction and forking are events *in* the log rather than
//! operations *on* it.
