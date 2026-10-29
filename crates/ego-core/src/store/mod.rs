//! Durable session storage: the codec and the `SessionStore` port.
//!
//! The codec is pure; the filesystem implementation lives in `ego-tools`.

pub mod blob;
pub mod codec;
pub mod evidence;
pub mod run_artifact;
pub mod session_store;
