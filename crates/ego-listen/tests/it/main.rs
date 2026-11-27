//! The one integration-test binary of `ego-listen`.
//!
//! Every file in `tests/` used to be its own binary, and each one linked the
//! whole dependency graph. One binary links it once. Each former binary is a
//! module here, so `cargo nextest run -p ego-listen --test it -E 'test(/^<file>::/)'`
//! selects what `--test <file>` selected before.

mod connection_limits;
mod connection_tasks;
mod local_endpoint;
mod windows_named_pipe;
