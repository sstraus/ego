//! Compose routing from trusted config and the current run's admitted HTTP ports.
use super::*;

pub(super) fn compose(
    http: Arc<dyn ego_ai::http::Http>,
    root: &Path,
) -> Option<Arc<dyn ego_core::turn::TurnRouter>> {
    let config = config::jev(config::ego_home().as_deref()).ok()?;
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(Arc::new(ego_ai::jev::router::JevRouter::new(
        config,
        home.into(),
        root.to_owned(),
        http,
    )))
}

pub(super) fn attach<'session: 'turn, 'turn>(
    loop_: TurnLoop<'session, 'turn>,
    router: Option<Arc<dyn ego_core::turn::TurnRouter>>,
) -> TurnLoop<'session, 'turn> {
    match router {
        Some(router) => loop_.with_router(router),
        None => loop_,
    }
}
