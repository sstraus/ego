//! The one integration-test binary of `ego-a2a`.
//!
//! Every file in `tests/` used to be its own binary, and each one linked the
//! whole dependency graph. One binary links it once. Each former binary is a
//! module here, so `cargo nextest run -p ego-a2a --test it -E 'test(/^<file>::/)'`
//! selects what `--test <file>` selected before.

mod connection_policy;
mod listener;
