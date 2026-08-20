//! Durable session storage: the codec and the `SessionStore` port.
//!
//! See `ARCHITECTURE.md` §5. The codec is pure; the filesystem implementation
//! lives in `ego-tools`.

pub mod codec;
