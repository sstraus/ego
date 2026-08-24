//! Honest unconfined backend.

use ego_core::permit::{BackendIdentity, ExecCaps};

use super::backend::{Backend, LaunchPrefix};

pub(crate) struct NullBackend;

impl Backend for NullBackend {
    fn identity(&self) -> BackendIdentity {
        BackendIdentity::new("null", env!("CARGO_PKG_VERSION"))
    }

    fn maximum_demonstrable_caps(&self) -> ExecCaps {
        ExecCaps::null()
    }

    fn launch_prefix(&self) -> LaunchPrefix {
        LaunchPrefix {
            program: None,
            args_before_target: Vec::new(),
        }
    }
}
