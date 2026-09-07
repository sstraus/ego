//! The process-level adapters every inbound host protocol needs.
//!
//! `ego acp` and `ego mcp-server` are two front doors onto the same process,
//! and each needs the same three things from it: a way to turn a path a peer
//! named into a [`RunRoot`], a way to spawn a run's execution onto the
//! runtime, and a source of fresh UUIDs. Both wrote their own, so there were
//! two `CanonicalRoots`, two `TokioExecutionSpawner`s and two `UuidIds` —
//! same names, same intent, and free to drift apart because nothing made them
//! agree.
//!
//! They are not ports in the sense
//! `docs/decisions/2026-09-07-a-port-needs-two-real-implementations.md` means:
//! nothing here is a boundary this crate owns. They are the *adapters* that
//! answer ports `ego-acp` and `ego-mcp` own, and one adapter can answer both.
//!
//! ## Why the root resolver keeps two error types and two sentences
//!
//! `RootResolver::resolve` returns [`RootResolveError`]; `McpRunRootResolver`
//! returns [`McpError`], which also carries a [`McpFailure`] classification a
//! peer acts on. Collapsing them would mean inventing a third error and
//! translating twice.
//!
//! The prose differs too, deliberately: an ACP host named a *working
//! directory*, an MCP client named a *run root*, and an operator reading
//! "the MCP run root … is not a directory" after an ACP request would be sent
//! looking in the wrong place. So the canonicalisation — the part that can
//! actually be got wrong — is written once in [`CanonicalRoots::canonicalize`],
//! and each protocol wraps it in its own error with its own noun.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use ego_acp::serve::{ExecutionSpawner, RootResolveError, RootResolver};
use ego_core::run::{IdSource, RunRoot};
use ego_mcp::{McpError, McpFailure, McpRunExecutionSpawner, McpRunRootResolver};

/// Resolves a path a peer named against the process's base directory.
pub struct CanonicalRoots {
    /// What a relative path is relative *to*. A peer's relative path is
    /// resolved against the process's own base, never against whatever
    /// directory the process happens to be sitting in when the request
    /// arrives.
    pub base: PathBuf,
}

impl CanonicalRoots {
    /// The whole of the shared logic: join, canonicalise, insist it is a
    /// directory, and hand back a [`RunRoot`].
    ///
    /// `subject` is how the caller's protocol names the thing, so the two
    /// front doors produce two accurate sentences from one implementation.
    /// Errors come back as plain strings because the two callers classify
    /// failure differently and neither can use the other's error type.
    fn canonicalize(&self, raw: &Path, subject: &str) -> Result<RunRoot, String> {
        let candidate = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.base.join(raw)
        };
        let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
            format!(
                "the {subject} {} could not be resolved: {error}",
                candidate.display()
            )
        })?;
        if !canonical.is_dir() {
            return Err(format!(
                "the {subject} {} is not a directory",
                canonical.display()
            ));
        }
        RunRoot::new(canonical).map_err(|error| error.to_string())
    }
}

impl RootResolver for CanonicalRoots {
    fn resolve(&self, cwd: &Path) -> Result<RunRoot, RootResolveError> {
        self.canonicalize(cwd, "ACP working directory")
            .map_err(RootResolveError::new)
    }
}

impl McpRunRootResolver for CanonicalRoots {
    fn resolve(&self, raw: &Path) -> Result<RunRoot, McpError> {
        self.canonicalize(raw, "MCP run root")
            .map_err(|diagnostic| McpError::new(McpFailure::InvalidValue, diagnostic))
    }
}

/// Spawns a run's execution onto the ambient Tokio runtime.
///
/// The two impls differ in one real way and it is kept: ACP spawns
/// unconditionally, while MCP asks for the current runtime handle first and
/// answers a [`McpFailure::Transport`] when there is none, because an MCP
/// client is owed an error rather than a panic when the process is not in a
/// position to run anything.
#[derive(Debug, Default)]
pub struct TokioExecutionSpawner;

impl ExecutionSpawner for TokioExecutionSpawner {
    fn spawn(&self, execution: Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
        tokio::spawn(execution);
    }
}

impl McpRunExecutionSpawner for TokioExecutionSpawner {
    fn spawn(
        &self,
        future: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<(), McpError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            McpError::new(
                McpFailure::Transport,
                format!("the MCP run execution runtime is unavailable: {error}"),
            )
        })?;
        runtime.spawn(future);
        Ok(())
    }
}

/// Fresh v4 UUIDs, the production [`IdSource`].
#[derive(Debug, Default)]
pub struct UuidIds;

impl IdSource for UuidIds {
    fn next_uuid(&self) -> uuid::Uuid {
        uuid::Uuid::new_v4()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Moved here from `acp.rs` when the two copies became one, and widened
    /// to cover both front doors. A single implementation is only an
    /// improvement if both callers still get the answer they got before —
    /// including the sentence, which is the part an operator reads.
    #[test]
    fn one_resolver_answers_both_front_doors_and_names_the_right_thing() {
        let owner = tempdir().expect("root owner");
        let directory = owner.path().join("workspace");
        std::fs::create_dir(&directory).expect("workspace directory");
        let regular_file = owner.path().join("workspace-file");
        std::fs::write(&regular_file, b"not a workspace").expect("workspace file");
        let roots = CanonicalRoots {
            base: owner.path().to_path_buf(),
        };
        let canonical = std::fs::canonicalize(&directory).expect("canonical workspace");

        assert_eq!(
            RootResolver::resolve(&roots, Path::new("workspace"))
                .expect("a directory is an ACP root")
                .as_path(),
            canonical.as_path()
        );
        assert_eq!(
            McpRunRootResolver::resolve(&roots, Path::new("workspace"))
                .expect("a directory is an MCP run root")
                .as_path(),
            canonical.as_path(),
            "both front doors resolve the same path to the same root"
        );

        let acp = RootResolver::resolve(&roots, Path::new("workspace-file"))
            .expect_err("an existing regular file is not an ACP root");
        let mcp = McpRunRootResolver::resolve(&roots, Path::new("workspace-file"))
            .expect_err("an existing regular file is not an MCP run root");
        assert!(
            acp.to_string().contains("ACP working directory"),
            "an ACP host is told which of its own inputs was wrong: {acp}"
        );
        assert!(
            mcp.to_string().contains("MCP run root"),
            "an MCP client is told which of its own inputs was wrong: {mcp}"
        );
        assert!(
            !acp.to_string().contains("MCP") && !mcp.to_string().contains("ACP"),
            "neither front door borrows the other's noun: {acp} / {mcp}"
        );
    }

    /// An absolute path a peer names is taken as given; a relative one is
    /// resolved against the process's base and never against the process's
    /// current directory, which nothing in a request controls.
    #[test]
    fn a_relative_path_resolves_against_the_base_and_an_absolute_one_stands() {
        let owner = tempdir().expect("root owner");
        let elsewhere = tempdir().expect("unrelated directory");
        let nested = owner.path().join("nested");
        std::fs::create_dir(&nested).expect("nested directory");
        let roots = CanonicalRoots {
            base: owner.path().to_path_buf(),
        };

        assert_eq!(
            RootResolver::resolve(&roots, Path::new("nested"))
                .expect("relative path resolves against the base")
                .as_path(),
            std::fs::canonicalize(&nested)
                .expect("canonical nested")
                .as_path()
        );
        assert_eq!(
            RootResolver::resolve(&roots, elsewhere.path())
                .expect("an absolute path is taken as given")
                .as_path(),
            std::fs::canonicalize(elsewhere.path())
                .expect("canonical elsewhere")
                .as_path()
        );
    }
}
