//! `ego`, the binary — argv, stdout, the process exit.
//!
//! `ARCHITECTURE.md` §3: this crate owns the command line, the two output
//! streams, signal handling and the process's exit status. It owns no
//! accounting, no projection and no exit-code table: each of those is a value
//! it reads from `ego-core`, which is what keeps an ACP host and this binary
//! saying the same thing about the same run.
//!
//! A library beside the binary so the parse and the assembly can be tested as
//! functions, and the *process* — exit codes, stdout, signals — can be tested
//! from outside by running it. Both are needed: `crates/ego-cli/tests/` runs
//! the built binary precisely because an exit code is not observable from
//! inside the process that produces it.

/// The product name shown to a person. Machine-facing names remain `ego`.
pub(crate) const DISPLAY_NAME: &str = "εgo";

pub mod acp;
pub mod auth;
pub mod budget;
pub mod catalogue;
pub mod cli;
pub mod config;
pub mod facts;
pub mod model;
pub mod model_picker;
pub mod models;
pub mod output;
pub mod prompt;
pub mod provider_http;
pub mod render;
pub mod repl;
pub mod run;
pub mod schema;
pub mod session;
