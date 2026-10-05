//! ACP observation lifecycle, separate from all model execution.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use agent_client_protocol_schema::v1;
use ego_acp::project::{AcpNotice, NoticeAction};
use ego_acp::serve::AcpNotices;
use ego_core::orchestration_events::SCAN_INTERVAL_MS;
use ego_core::store::session_store::SessionStore;
use ego_tools::orchestration_events::{ObservationPump, millis};
use ego_tools::store::JsonlStore;

pub(crate) struct ObserverGuard(tokio::task::AbortHandle);

impl Drop for ObserverGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) fn start(
    coordinator: bool,
    root: &Path,
    notices: AcpNotices,
    store: Arc<JsonlStore>,
) -> Result<Option<ObserverGuard>, String> {
    if !coordinator || std::env::var_os("TUIC_SOCKET").is_none_or(|socket| socket.is_empty()) {
        return Ok(None);
    }
    let home = std::env::var_os("HOME").ok_or("observer needs HOME")?;
    let mut pump = ObservationPump::open(Path::new(&home), root).map_err(|e| e.to_string())?;
    let root = root.to_owned();
    let task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(SCAN_INTERVAL_MS));
        let mut registered = false;
        loop {
            interval.tick().await;
            let now = millis(SystemTime::now());
            if !registered {
                match pump.register().await {
                    Ok(()) => registered = true,
                    Err(error) => {
                        pump.failure(error.to_string(), now);
                        if let Err(error) = pump.commit() {
                            tracing::error!(%error, "observer cursor commit failed");
                            break;
                        }
                    }
                }
            }
            if registered && let Err(error) = pump.poll(now).await {
                pump.failure(error.to_string(), now);
                if let Err(error) = pump.commit() {
                    tracing::error!(%error, "observer cursor commit failed");
                    break;
                }
            }
            if pump.ready(now).is_empty() {
                continue;
            }
            let sessions = match store.list() {
                Ok(sessions) => sessions,
                Err(error) => {
                    tracing::error!(%error, "observer cannot list ACP sessions");
                    continue;
                }
            };
            for session in sessions
                .iter()
                .filter(|session| session.workspace_root == root)
            {
                let seat = v1::SessionId::new(session.id.to_string());
                let delivered = pump.ready(now).iter().all(|card| {
                    notices
                        .publish(
                            &seat,
                            &AcpNotice {
                                id: card.id.clone(),
                                text: card.text.clone(),
                                action: match &card.result {
                                    Some(path) => NoticeAction::OpenResult { path: path.clone() },
                                    None => NoticeAction::OpenResult {
                                        path: pump.cursor_path().to_owned(),
                                    },
                                },
                            },
                        )
                        .is_ok()
                });
                if delivered {
                    if let Err(error) = pump.acknowledge() {
                        tracing::error!(%error, "observer delivery commit failed");
                        return;
                    }
                    break;
                }
            }
        }
    });
    Ok(Some(ObserverGuard(task.abort_handle())))
}
