//! Terminal revocation tears down effect ownership before same-session recomposition.
use super::*;

pub(super) fn requested(
    arguments: &str,
    seat: &dyn InteractiveSeat,
    state: &SessionState,
    cwd: &Path,
) -> Result<CanonPath, String> {
    if !seat.has_human() {
        return Err("root revocation requires a human terminal".into());
    }
    let path = arguments.strip_prefix("roots revoke ").unwrap_or("").trim();
    if path.is_empty() {
        return Err("usage: /config roots revoke EXACT_PATH".into());
    }
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    // A removed root must remain revocable. Match its saved canonical spelling
    // even when canonicalization can no longer reach the filesystem object.
    let path = CanonPath::new(std::fs::canonicalize(&path).unwrap_or(path));
    if !state
        .session_roots
        .iter()
        .chain(
            state
                .permanent_root_consents
                .iter()
                .map(|consent| &consent.root),
        )
        .any(|root| root.path() == &path)
    {
        return Err(format!(
            "no saved root grant for {}",
            ego_core::permit::terminal_safe(&path.to_string())
        ));
    }
    Ok(path)
}

pub(super) fn revoke(
    active: &mut ActiveSession<'_>,
    path: CanonPath,
    args: &Args,
    clock: &SystemClock,
    sink: &dyn EventSink,
    store: &JsonlStore,
) -> Result<ego_core::store::session_store::SessionMeta, RunTermination> {
    let failure = |message: String| RunTermination::Failed { message };
    let permanent = active
        .state()
        .permanent_root_consents
        .iter()
        .any(|consent| consent.root.path() == &path);
    let mut journal = Journal::new(
        active.id(),
        active.store(),
        clock,
        sink,
        Producer::Main,
        active.state().clone(),
    );
    // Journal first: a config write failure can never resurrect this session's consent.
    journal
        .append(
            TurnId::new(),
            None,
            None,
            EventKind::RootRevoked { path: path.clone() },
        )
        .map_err(|error| failure(error.to_string()))?;
    let state = journal.state().clone();
    drop(journal);
    active.replace_state(state);
    if permanent {
        let target = config::profile(args).map_or(UserConfigTarget::Base, |profile| {
            UserConfigTarget::Profile(
                ego_core::config::ProfileName::parse(profile.name())
                    .expect("selected profile was validated at admission"),
            )
        });
        let receipt = crate::config::user_store()
            .map_err(|error| failure(format!("{error:?}")))?
            .mutate(UserConfigMutation::Root {
                target,
                path: path.as_path().to_path_buf(),
                access: None,
            })
            .map_err(|error| failure(error.to_string()))?;
        if let UserConfigPublication::PublishedDurabilityUncertain { diagnostic } =
            receipt.publication
        {
            return Err(failure(format!(
                "revoked config durability is uncertain: {diagnostic}"
            )));
        }
    }
    store
        .readable_meta(&active.id())
        .map_err(|error| failure(error.to_string()))?
        .ok_or_else(|| failure("revoked session metadata is unavailable".into()))
}
