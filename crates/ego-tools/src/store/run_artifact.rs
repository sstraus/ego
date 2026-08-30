//! Atomic filesystem adapter for already-public run artifacts.

use std::fs::{self, OpenOptions};

use ego_core::run::RunId;
use ego_core::store::run_artifact::{
    ArtifactPublication, PublishedRunArtifact, RunArtifactId, RunArtifactStore,
    RunArtifactStoreError,
};
use fd_lock::RwLock;
use serde::{Deserialize, Serialize};

use super::JsonlStore;
use super::io::AtomicReplacePublication;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRunArtifact {
    artifact_id: String,
    mime_type: String,
    bytes: Vec<u8>,
}

impl StoredRunArtifact {
    fn from_public(artifact: &PublishedRunArtifact) -> Self {
        Self {
            artifact_id: artifact.id().as_str().to_owned(),
            mime_type: artifact.mime_type().to_owned(),
            bytes: artifact.bytes().to_vec(),
        }
    }

    fn into_public(self, artifact_id: RunArtifactId) -> PublishedRunArtifact {
        if artifact_id == RunArtifactId::sanitized_bundle() {
            PublishedRunArtifact::sanitized_bundle(self.bytes)
        } else {
            PublishedRunArtifact::output(self.mime_type, self.bytes)
        }
    }
}

impl JsonlStore {
    fn artifact_path(&self, run_id: RunId, artifact_id: RunArtifactId) -> std::path::PathBuf {
        self.session_dir(&run_id.session_id())
            .join("artifacts")
            .join(run_id.nonce().to_string())
            .join(format!("{}.json", artifact_id.as_str()))
    }

    fn artifact_lock_path(&self, run_id: RunId) -> std::path::PathBuf {
        self.session_dir(&run_id.session_id())
            .join("artifacts")
            .join(run_id.nonce().to_string())
            .join("publication.lock")
    }

    fn with_artifact_lock<T>(
        &self,
        run_id: RunId,
        operation: impl FnOnce() -> Result<T, RunArtifactStoreError>,
    ) -> Result<T, RunArtifactStoreError> {
        let lock_path = self.artifact_lock_path(run_id);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(artifact_io)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(artifact_io)?;
        let mut lock = RwLock::new(file);
        let _guard = lock.write().map_err(artifact_io)?;
        operation()
    }
}

impl RunArtifactStore for JsonlStore {
    fn publish(
        &self,
        run_id: RunId,
        artifact: PublishedRunArtifact,
    ) -> Result<ArtifactPublication, RunArtifactStoreError> {
        let artifact_id = artifact.id();
        self.with_artifact_lock(run_id, || {
            let path = self.artifact_path(run_id, artifact_id);
            let requested = StoredRunArtifact::from_public(&artifact);
            if let Some(existing) = self.io.read(&path).map_err(artifact_io)? {
                let existing =
                    serde_json::from_slice::<StoredRunArtifact>(&existing).map_err(artifact_io)?;
                if existing.artifact_id != artifact_id.as_str() {
                    return Err(RunArtifactStoreError::Conflict {
                        run_id,
                        artifact_id,
                    });
                }
                if existing == requested {
                    return Ok(ArtifactPublication::PublishedDurably);
                }
                if artifact_id == RunArtifactId::sanitized_bundle() {
                    return Err(RunArtifactStoreError::Conflict {
                        run_id,
                        artifact_id,
                    });
                }
            }

            let bytes = serde_json::to_vec(&requested).map_err(artifact_io)?;
            match self
                .io
                .replace_atomic_publication(&path, &bytes)
                .map_err(artifact_io)?
            {
                AtomicReplacePublication::PublishedDurably => {
                    Ok(ArtifactPublication::PublishedDurably)
                }
                AtomicReplacePublication::PublishedDurabilityUncertain { diagnostic } => {
                    Ok(ArtifactPublication::PublishedDurabilityUncertain { diagnostic })
                }
            }
        })
    }

    fn read(
        &self,
        run_id: RunId,
        artifact_id: RunArtifactId,
    ) -> Result<PublishedRunArtifact, RunArtifactStoreError> {
        let path = self.artifact_path(run_id, artifact_id);
        let Some(bytes) = self.io.read(&path).map_err(artifact_io)? else {
            return Err(RunArtifactStoreError::Absent {
                run_id,
                artifact_id,
            });
        };
        let stored = serde_json::from_slice::<StoredRunArtifact>(&bytes).map_err(artifact_io)?;
        if stored.artifact_id != artifact_id.as_str() {
            return Err(RunArtifactStoreError::Conflict {
                run_id,
                artifact_id,
            });
        }
        if artifact_id == RunArtifactId::sanitized_bundle()
            && stored.mime_type != "application/json"
        {
            return Err(RunArtifactStoreError::Conflict {
                run_id,
                artifact_id,
            });
        }
        Ok(stored.into_public(artifact_id))
    }
}

fn artifact_io(error: impl std::fmt::Display) -> RunArtifactStoreError {
    RunArtifactStoreError::Io {
        message: error.to_string(),
    }
}
