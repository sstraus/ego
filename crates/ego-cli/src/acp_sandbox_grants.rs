//! Workspace sandbox grants a person asked ACP to keep.
//!
//! One JSON document under the ego home, keyed by canonical workspace. It is
//! not the workspace trust ledger: trusting a project's instructions and
//! letting its commands reach a path are different grants, and neither implies
//! the other.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use ego_acp::sandbox::SandboxGrantStore;
use ego_core::permit::{CanonPath, SandboxAccess, SandboxGrant};
use serde::{Deserialize, Serialize};

use crate::trust::LockedJsonStore;

const NAME: &str = "sandbox-grants";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    grants: Vec<Stored>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stored {
    workspace: PathBuf,
    path: PathBuf,
    access: SandboxAccess,
}

pub struct FileSandboxGrants {
    home: PathBuf,
}

impl std::fmt::Debug for FileSandboxGrants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSandboxGrants")
            .field("home", &self.home)
            .finish()
    }
}

impl FileSandboxGrants {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    fn store(&self) -> LockedJsonStore<Ledger> {
        LockedJsonStore::new(&self.home, NAME)
    }
}

impl SandboxGrantStore for FileSandboxGrants {
    fn load(&self, workspace: &Path) -> Vec<SandboxGrant> {
        match self.store().read() {
            Ok(ledger) => ledger
                .grants
                .into_iter()
                .filter(|stored| stored.workspace == workspace)
                .map(|stored| SandboxGrant::new(CanonPath::new(stored.path), stored.access))
                .collect(),
            Err(error) => {
                // An unreadable document grants nothing.
                tracing::warn!(%error, "stored sandbox grants could not be read");
                Vec::new()
            }
        }
    }

    fn remember<'a>(
        &'a self,
        workspace: &'a Path,
        grant: &'a SandboxGrant,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        let store = self.store();
        let stored = Stored {
            workspace: workspace.to_path_buf(),
            path: grant.path.as_path().to_path_buf(),
            access: grant.access,
        };
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                store.update(|ledger| {
                    if !ledger.grants.contains(&stored) {
                        ledger.grants.push(stored);
                    }
                })
            })
            .await
            .map_err(|error| format!("the sandbox grant writer task failed: {error}"))?
        })
    }
}
