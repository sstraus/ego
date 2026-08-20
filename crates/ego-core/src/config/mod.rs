//! Strict configuration and the trust gate.
//!
//! See `ARCHITECTURE.md` and `plans/m0a-contracts.md` Step 12. Resolution is a
//! pure function over injected sources — no file I/O lives in core.

mod error;
mod layer;
mod resolve;
mod source;

pub use error::ConfigError;
pub use layer::{ConfigLayer, Mode, ModelRef, PermCfg, ProfileLayer};
pub use resolve::{Effective, Posture, resolve};
pub use source::{Source, Sourced, Trust};
