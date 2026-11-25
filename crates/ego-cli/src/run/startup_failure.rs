//! Diagnostic evidence for a launch that never publishes its first session state.

use std::path::PathBuf;

use ego_core::event::id::SessionId;
use ego_tools::store::{OsIo, StoreIo};

use super::{Diagnostics, RunTermination};

pub(super) struct StartupFailure<'a> {
    directory: PathBuf,
    path: PathBuf,
    session: SessionId,
    phase: &'static str,
    diagnostics: &'a dyn Diagnostics,
    finished: bool,
}

impl<'a> StartupFailure<'a> {
    pub(super) fn new(
        root: &std::path::Path,
        session: SessionId,
        diagnostics: &'a dyn Diagnostics,
    ) -> Self {
        let directory = root.join(session.to_string());
        Self {
            path: directory.join(format!("startup-{}.json", SessionId::new())),
            directory,
            session,
            phase: "prepare resume",
            diagnostics,
            finished: false,
        }
    }

    pub(super) fn phase(&mut self, phase: &'static str) {
        self.phase = phase;
        if self.path.is_file() {
            self.opened();
        }
    }

    /// Called with the writer lease held, before any further startup work.
    pub(super) fn opened(&self) {
        self.record(
            "unresolved",
            "startup has not published session state",
            None,
        );
    }

    fn unpublished(&self) -> bool {
        self.directory.join("lease").is_file()
            && !self.directory.join("manifest.json").exists()
            && !self.directory.join("events.jsonl").exists()
    }

    fn record(&self, outcome: &str, message: &str, exit_code: Option<u8>) {
        if !self.unpublished() {
            self.remove();
            return;
        }
        let bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "session": self.session.to_string(),
            "pid": std::process::id(),
            "phase": self.phase,
            "outcome": outcome,
            "message": message,
            "exit_code": exit_code,
        }))
        .expect("startup diagnostic contains only JSON strings and a process id");
        if let Err(error) = OsIo.replace_atomic_private(&self.path, &bytes) {
            self.diagnostics.note(&format!(
                "could not preserve startup diagnostic at {}: {error}",
                self.path.display()
            ));
        }
    }

    fn remove(&self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            self.diagnostics.note(&format!(
                "could not remove startup diagnostic at {}: {error}",
                self.path.display()
            ));
        }
    }

    pub(super) fn finish(&mut self, error: Option<&RunTermination>) {
        match error {
            Some(error) => {
                let message = match error {
                    RunTermination::Failed { message }
                    | RunTermination::UsageError { message }
                    | RunTermination::Guard { message }
                    | RunTermination::Provider { message }
                    | RunTermination::ProviderUncertain { message }
                    | RunTermination::ContextOverflow { message } => message.as_str(),
                    _ => error.exit_reason(),
                };
                self.record("failed", message, Some(error.exit_code()));
            }
            None => self.remove(),
        }
        self.finished = true;
    }
}

impl Drop for StartupFailure<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // Do not guess an exit code for a panic or a dropped startup future.
            self.record(
                "unresolved",
                "startup ended without a returned outcome",
                None,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_tools::clock::SystemClock;
    use ego_tools::store::JsonlStore;

    struct Silent;
    impl Diagnostics for Silent {
        fn note(&self, message: &str) {
            panic!("unexpected diagnostic: {message}");
        }
    }

    // Catches: abandoning a leased startup loses its only diagnostic, or a
    // successful invocation is left looking like an unresolved failure.
    #[test]
    fn unpublished_startup_is_retained_on_abandonment_but_not_success() {
        for successful in [false, true] {
            let tree = tempfile::tempdir().unwrap();
            let id = SessionId::new();
            let store = JsonlStore::new(tree.path().join("sessions"), tree.path(), Box::new(OsIo));
            let _opened = store
                .open_writer(&id, crate::session::lease_inputs(&SystemClock))
                .unwrap();
            let mut diagnostic = StartupFailure::new(store.root(), id, &Silent);
            let path = diagnostic.path.clone();
            diagnostic.opened();
            let pending: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(pending["outcome"], "unresolved");
            assert!(pending["exit_code"].is_null());
            diagnostic.phase("admit context");
            if successful {
                diagnostic.finish(None);
            }
            drop(diagnostic);
            if successful {
                assert!(!path.exists(), "successful startup left a false failure");
            } else {
                let retained: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                assert_eq!(retained["outcome"], "unresolved");
                assert_eq!(retained["phase"], "admit context");
                assert_eq!(
                    retained["message"],
                    "startup ended without a returned outcome"
                );
                assert!(retained["exit_code"].is_null());
            }
        }
    }

    // Catches: reporting an error before a lease creates a phantom session.
    #[test]
    fn prelease_failure_does_not_create_session_files() {
        let tree = tempfile::tempdir().unwrap();
        let mut diagnostic = StartupFailure::new(tree.path(), SessionId::new(), &Silent);
        diagnostic.finish(Some(&RunTermination::Failed {
            message: "before lease".into(),
        }));
        assert!(!diagnostic.directory.exists());
    }
}
