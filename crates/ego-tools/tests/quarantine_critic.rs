//! Cross-consumer and attachment races for refused ACP sessions.

use std::path::Path;
use std::sync::Arc;

use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::run::RequestId;
use ego_core::session_lifecycle::{
    ActivityFuture, CloseFuture, ClosedSession, HoldSignalFuture, LifecycleError, SessionActivity,
    SessionLifecycle, SessionLifecycleStore, SessionRunLifecycle,
};
use ego_core::store::session_store::SessionStore;
use ego_tools::store::{JsonlStore, OsIo};

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(1000)
    }
}

struct Runs {
    quarantine_during_activity: Option<Arc<JsonlStore>>,
}

impl SessionRunLifecycle for Runs {
    fn activity(&self, session: SessionId) -> ActivityFuture<'_> {
        Box::pin(async move {
            if let Some(store) = &self.quarantine_during_activity {
                store.quarantine(session)?;
            }
            Ok(SessionActivity::Inactive)
        })
    }

    fn cancel_and_settle(&self, session: SessionId) -> CloseFuture<'_> {
        Box::pin(std::future::ready(Ok(ClosedSession {
            id: session,
            cancelled: true,
        })))
    }

    fn hold(&self, _session: SessionId, _request: RequestId) -> HoldSignalFuture<'_> {
        Box::pin(std::future::ready(Ok(false)))
    }
}

fn store(root: &Path, workspace: &Path) -> Arc<JsonlStore> {
    Arc::new(JsonlStore::new(root, workspace, Box::new(OsIo)))
}

fn lifecycle(store: Arc<JsonlStore>) -> SessionLifecycle<JsonlStore> {
    SessionLifecycle::new(
        store,
        Arc::new(Runs {
            quarantine_during_activity: None,
        }),
        Arc::new(FixedClock),
    )
}

// Catches: CLI catalogue readers bypass lifecycle filtering and expose refused sessions,
// including when a missing manifest makes listing fall back to the event log.
#[test]
fn quarantine_does_not_leak_through_raw_catalogue() {
    catalogue_case(false);
}

// Catches: manifest recovery bypasses quarantine when the cache is missing.
#[test]
fn quarantine_does_not_leak_through_manifest_recovery() {
    catalogue_case(true);
}

fn catalogue_case(missing_manifest: bool) {
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let disk = store(&root, dir.path());
        let sessions = lifecycle(disk.clone());
        let refused = sessions.create(dir.path().into()).unwrap().id;
        let neighbour = sessions.create(dir.path().into()).unwrap().id;
        sessions.quarantine(refused).unwrap();
        if missing_manifest {
            std::fs::remove_file(root.join(refused.to_string()).join("manifest.json")).unwrap();
        }
        let reopened = store(&root, dir.path());
        let ids: Vec<_> = reopened
            .list()
            .unwrap()
            .iter()
            .map(|meta| meta.id)
            .collect();
        assert_eq!(
            ids,
            vec![neighbour],
            "raw catalogue exposes quarantine (missing manifest: {missing_manifest})"
        );
    }
}

// Catches: load/resume check the marker before awaiting activity and return an attachment
// after another connection has completed quarantining the same session.
#[tokio::test]
async fn quarantine_completed_during_activity_cannot_return_a_load_attachment() {
    attachment_case(true).await;
}

// Catches: resume returns an attachment after quarantine during its activity await.
#[tokio::test]
async fn quarantine_completed_during_activity_cannot_return_a_resume_attachment() {
    attachment_case(false).await;
}

async fn attachment_case(replay: bool) {
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let disk = store(&root, dir.path());
        let id = lifecycle(disk.clone())
            .create(dir.path().into())
            .unwrap()
            .id;
        let other_connection = store(&root, dir.path());
        let sessions = SessionLifecycle::new(
            disk,
            Arc::new(Runs {
                quarantine_during_activity: Some(other_connection.clone()),
            }),
            Arc::new(FixedClock),
        );
        let result = if replay {
            sessions.load(id, dir.path()).await
        } else {
            sessions.resume(id, dir.path()).await
        };
        assert!(other_connection.is_quarantined(id).unwrap());
        assert!(
            matches!(result, Err(LifecycleError::UnknownSession)),
            "attachment escaped completed quarantine (load: {replay}): {result:?}"
        );
    }
}

// Catches: publishing under a quarantined id overwrites its log or clears its marker,
// resurrecting a refused session while the caller receives a successful new session.
#[test]
fn reusing_a_quarantined_session_id_is_refused_and_keeps_neighbours_visible() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let disk = store(&root, dir.path());
    let sessions = lifecycle(disk.clone());
    let id = sessions.create(dir.path().into()).unwrap().id;
    let neighbour = sessions.create(dir.path().into()).unwrap().id;
    sessions.quarantine(id).unwrap();
    let before = std::fs::read(root.join(id.to_string()).join("events.jsonl")).unwrap();
    assert!(sessions.create_with_id(id, dir.path().into()).is_err());
    assert_eq!(
        std::fs::read(root.join(id.to_string()).join("events.jsonl")).unwrap(),
        before
    );
    assert!(disk.is_quarantined(id).unwrap());
    let page = sessions.list(None, 100, None).unwrap();
    assert_eq!(
        page.items.iter().map(|meta| meta.id).collect::<Vec<_>>(),
        vec![neighbour]
    );
}
