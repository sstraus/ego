//! Process-level confinement diagnostics shared across protocol startup and sessions.

use std::sync::{Arc, Mutex, PoisonError};

use ego_core::permit::{ExecCaps, ProcessError};

use crate::output::Diagnostics;

pub(crate) struct Reporter {
    diagnostics: Arc<dyn Diagnostics>,
    warned: Mutex<bool>,
}

impl Reporter {
    pub(crate) fn new(diagnostics: Arc<dyn Diagnostics>) -> Self {
        Self {
            diagnostics,
            warned: Mutex::new(false),
        }
    }

    pub(crate) fn report(&self, result: Result<ExecCaps, ProcessError>) {
        match result {
            Ok(capabilities) if capabilities != ExecCaps::empty() => {}
            Ok(_) => {
                let mut warned = self.warned.lock().unwrap_or_else(PoisonError::into_inner);
                if *warned {
                    return;
                }
                self.diagnostics
                    .note("the process backend provides no effective confinement");
                *warned = true;
            }
            Err(error) => self.diagnostics.note(&format!(
                "the process confinement diagnostic could not be completed: {error}"
            )),
        }
    }
}

#[cfg(test)]
mod tests;
