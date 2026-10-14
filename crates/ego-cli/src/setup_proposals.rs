//! Deterministic read-root proposals, resolved through the real configuration layers.

use std::path::Path;
use std::sync::Arc;

use ego_core::config::{
    ConfigLayer, Effective, ManagedConfigKey, ManagedConfigValue, ProfileName, Source, Sourced,
    Trust, UserConfigBatch, UserConfigMutation, UserConfigStore, UserConfigTarget,
    resolve_selected,
};
use ego_core::permit::{
    CanonPath, Classification, Op, PolicyEngine, PreparedCall, Resolution, RootAccess, RootCfg,
};
use ego_core::setup::{Finding, Probe, Status};
use ego_tools::config::OsUserConfigStore;
use ego_tools::store::{OsIo, StoreIo};

pub(crate) struct Preview {
    pub batch: Box<dyn UserConfigBatch>,
    pub items: Vec<usize>,
    pub paths: Vec<std::path::PathBuf>,
    pub target: UserConfigTarget,
    pub layers: Vec<Sourced<ConfigLayer>>,
    pub identities: Vec<String>,
    pub environment: Vec<(String, Option<std::ffi::OsString>)>,
    pub stored_before: Vec<RootCfg>,
    pub stored_after: Vec<RootCfg>,
}

fn root_changes(target: &UserConfigTarget, roots: &[RootCfg]) -> Vec<UserConfigMutation> {
    roots
        .iter()
        .map(|root| UserConfigMutation::Root {
            target: target.clone(),
            path: root.path.clone(),
            access: Some(root.access),
        })
        .collect()
}

fn read_admitted(effective: &Effective, workspace: &CanonPath, path: &CanonPath) -> bool {
    let Ok(roots) = crate::run::profile_roots::admit(effective, workspace) else {
        return false;
    };
    let policy = PolicyEngine::new(
        workspace.clone(),
        effective.mode.unwrap_or(ego_core::config::Mode::Default),
        effective.permissions.clone(),
    )
    .with_profile_roots(roots);
    match policy.classify(PreparedCall::new("read", Op::Read { path: path.clone() })) {
        Classification::Settled(Ok(_)) => true,
        Classification::Ask(pending) => matches!(
            futures::executor::block_on(policy.settle(pending)),
            Resolution::Settled(Ok(_))
        ),
        _ => false,
    }
}

fn resolve_candidate(
    layers: &[Sourced<ConfigLayer>],
    candidate: &str,
    workspace: &CanonPath,
) -> Result<Effective, String> {
    let mut layers = layers.to_vec();
    layers.retain(|layer| layer.source != Source::User);
    layers.push(ConfigLayer::parse(Source::User, candidate).map_err(|error| error.to_string())?);
    let env_profile = std::env::var("EGO_PROFILE").ok();
    let mut effective = resolve_selected(
        &layers,
        Trust::Untrusted,
        env_profile.as_deref().map(|name| (name, Source::Env)),
    )
    .map_err(|error| error.to_string())?;
    if let Some(home) = std::env::var_os("HOME") {
        crate::run::profile_roots::defaults(&mut effective, Path::new(&home));
    }
    crate::run::profile_roots::admit(&effective, workspace)?;
    Ok(effective)
}

/// No model-selected TOML, rule wildcard, mode, sandbox, trust or network change.
/// This initial mapper proposes only read reach. Other consumer gaps remain explicit.
pub(crate) fn preview(
    layers: &[Sourced<ConfigLayer>],
    effective: &Effective,
    workspace: &CanonPath,
    home: &Path,
    profile: Option<&str>,
    probes: &[Probe],
    findings: &[Finding],
) -> Result<Option<Preview>, String> {
    let target = match profile {
        Some(name) => {
            UserConfigTarget::Profile(ProfileName::parse(name).map_err(|error| error.to_string())?)
        }
        None => UserConfigTarget::Base,
    };
    let store =
        OsUserConfigStore::new(Arc::new(OsIo) as Arc<dyn StoreIo>, home.join("config.toml"));
    let snapshot = store
        .snapshot(target.clone())
        .map_err(|error| error.to_string())?;
    let stored = snapshot
        .values
        .iter()
        .find_map(|(key, value)| match (key, value) {
            (ManagedConfigKey::Roots, Some(ManagedConfigValue::Roots(roots))) => {
                Some(roots.clone())
            }
            _ => None,
        })
        .unwrap_or_default();
    let mut roots = stored.clone();
    // Roots replace a layer list. Preserve the already effective lower-layer reach.
    for root in &effective.roots {
        if !roots.contains(root) {
            roots.push(root.clone());
        }
    }
    let mut items = Vec::new();
    let mut paths = Vec::new();
    for finding in findings {
        if !matches!(finding.status, Status::Blocked) {
            continue;
        }
        if !matches!(
            probes.get(finding.id),
            Some(Probe::Read { .. } | Probe::Instructions { .. } | Probe::Git { .. })
        ) {
            continue;
        }
        let Some(path) = &finding.canonical else {
            continue;
        };
        let Some(directory) = path.parent() else {
            continue;
        };
        // No ancestor of the workspace and no config control directory exposure.
        if workspace.as_path().starts_with(directory)
            || home.starts_with(directory)
            || directory.starts_with(home)
        {
            continue;
        }
        let root = RootCfg {
            path: directory.to_path_buf(),
            access: RootAccess::Read,
        };
        let mut proposed = roots.clone();
        if !proposed.contains(&root) {
            proposed.push(root);
        }
        let batch = store
            .prepare_batch(root_changes(&target, &proposed))
            .map_err(|error| error.to_string())?;
        let Ok(after) = resolve_candidate(layers, batch.candidate(), workspace) else {
            continue;
        };
        // Higher layers, explicit deny, floors and ceilings cannot be lifted.
        if after.permissions != effective.permissions
            || after.mode != effective.mode
            || after.sandbox != effective.sandbox
            || after.network != effective.network
            || after.exec_paths != effective.exec_paths
        {
            continue;
        }
        if !effective
            .roots
            .iter()
            .all(|root| after.roots.contains(root))
        {
            continue;
        }
        if !read_admitted(&after, workspace, &CanonPath::new(path)) {
            continue;
        }
        roots = proposed;
        items.push(finding.id);
        paths.push(finding.requested.clone());
    }
    if items.is_empty() {
        return Ok(None);
    }
    let batch = store
        .prepare_batch(root_changes(&target, &roots))
        .map_err(|error| error.to_string())?;
    let after = resolve_candidate(layers, batch.candidate(), workspace)?;
    if paths
        .iter()
        .any(|path| !read_admitted(&after, workspace, &CanonPath::new(path)))
    {
        return Err("combined setup proposal is ineffective".into());
    }
    let identities = paths
        .iter()
        .map(|path| path_identity(path))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(Preview {
        batch,
        items,
        paths,
        target,
        layers: layers.to_vec(),
        identities,
        environment: environment_identity(),
        stored_before: stored,
        stored_after: roots,
    }))
}

fn path_identity(path: &Path) -> Result<String, String> {
    use ego_core::setup::ProbeReader;
    let observation = ego_tools::setup::NativeProbeReader::capture().metadata(path)?;
    Ok(format!(
        "{}:{}",
        observation.canonical, observation.identity
    ))
}

/// Only the private host dispatch can ask the seat; the batch is consumed once.
pub(crate) fn confirm_apply(
    preview: Preview,
    workspace: &CanonPath,
    home: &Path,
    confirm: &dyn Fn(String) -> bool,
) -> Result<String, crate::cli::UsageError> {
    use ego_core::config::UserConfigError;
    use sha2::{Digest, Sha256};
    let setup_id = ego_core::event::id::ItemId::new();
    let base = format!("{:x}", Sha256::digest(preview.batch.before().as_bytes()));
    let candidate = format!("{:x}", Sha256::digest(preview.batch.candidate().as_bytes()));
    let question = format!(
        "Setup {setup_id}: items {:?}, target {:?}\nRead-directory exposure includes siblings/descendants. Stored roots {:?} -> {:?}\nPaths {:?}\nBase {base}, candidate {candidate}. Apply? [y/N] ",
        preview.items, preview.target, preview.stored_before, preview.stored_after, preview.paths
    );
    if !confirm(question) {
        return Ok("Setup declined/cancelled; no configuration writes.\n".into());
    }
    let check = || {
        let current =
            crate::config::base_layers_in(workspace.as_path(), Trust::Untrusted, Some(home))
                .map_err(|error| UserConfigError::new(error.to_string()))?;
        let selected = std::env::var("EGO_PROFILE").ok();
        let selection = selected.as_deref().map(|name| (name, Source::Env));
        let current_policy = resolve_selected(&current, Trust::Untrusted, selection)
            .map_err(|error| UserConfigError::new(error.to_string()))?;
        let preview_policy = resolve_selected(&preview.layers, Trust::Untrusted, selection)
            .map_err(|error| UserConfigError::new(error.to_string()))?;
        if current_policy != preview_policy || environment_identity() != preview.environment {
            return Err(UserConfigError::new(
                "setup policy changed; confirm a fresh preview",
            ));
        }
        for (path, identity) in preview.paths.iter().zip(&preview.identities) {
            if path_identity(path).map_err(UserConfigError::new)? != *identity {
                return Err(UserConfigError::new(
                    "setup path changed; confirm a fresh preview",
                ));
            }
        }
        Ok(())
    };
    let store =
        OsUserConfigStore::new(Arc::new(OsIo) as Arc<dyn StoreIo>, home.join("config.toml"));
    let paths = preview.paths.clone();
    let result = store
        .publish_batch_checked(preview.batch, &check)
        .map_err(|error| crate::cli::UsageError(error.to_string()))?;
    let layers = crate::config::base_layers_in(workspace.as_path(), Trust::Untrusted, Some(home))?;
    let env_profile = std::env::var("EGO_PROFILE").ok();
    let mut effective = resolve_selected(
        &layers,
        Trust::Untrusted,
        env_profile.as_deref().map(|name| (name, Source::Env)),
    )
    .map_err(|error| crate::cli::UsageError(error.to_string()))?;
    if let Some(home) = std::env::var_os("HOME") {
        crate::run::profile_roots::defaults(&mut effective, Path::new(&home));
    }
    let mut report = format!(
        "Setup publication: {:?}; resume before using changed authority.\n",
        result.publication
    );
    for path in paths {
        let status = if path_identity(&path).is_err() {
            "unknown"
        } else if read_admitted(&effective, workspace, &CanonPath::new(&path)) {
            "fixed (policy-only)"
        } else {
            "still blocked"
        };
        report.push_str(&format!(
            "Re-probe {}: {status}; existing run authority unchanged.\n",
            ego_core::permit::terminal_safe(&path.display().to_string())
        ));
    }
    Ok(report)
}

fn environment_identity() -> Vec<(String, Option<std::ffi::OsString>)> {
    [
        "HOME",
        "EGO_HOME",
        "EGO_PROFILE",
        "PATH",
        "TMPDIR",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "DEVELOPER_DIR",
    ]
    .into_iter()
    .map(|name| (name.to_owned(), std::env::var_os(name)))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::setup::Finding;

    type Fixture = (
        tempfile::TempDir,
        CanonPath,
        std::path::PathBuf,
        Vec<Sourced<ConfigLayer>>,
        Effective,
        Vec<Probe>,
        Vec<Finding>,
    );

    fn fixture(extra: &str) -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let sibling = temp.path().join("needed");
        let home = temp.path().join("ego-home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        std::fs::create_dir(&home).unwrap();
        let path = sibling.join("task.md");
        std::fs::write(&path, "task evidence").unwrap();
        let text = format!("roots=[]\n{extra}");
        std::fs::write(home.join("config.toml"), &text).unwrap();
        let layers = vec![ConfigLayer::parse(Source::User, &text).unwrap()];
        let effective = resolve_selected(&layers, Trust::Untrusted, None).unwrap();
        let probes = vec![Probe::Read {
            path: path.clone(),
            reason: "task".into(),
        }];
        let findings = vec![Finding {
            id: 0,
            reason: "task".into(),
            requested: path.clone(),
            canonical: Some(path),
            status: Status::Blocked,
            evidence: "outside read reach".into(),
            observed: false,
            truncated: false,
        }];
        (
            temp,
            CanonPath::new(workspace),
            home,
            layers,
            effective,
            probes,
            findings,
        )
    }

    // Catches: a dry-run persists config or replaces the original list without improving read admission.
    #[test]
    fn setup_root_preview_is_effective_and_never_publishes() {
        let (_temp, workspace, home, layers, effective, probes, findings) = fixture("");
        let root = RootCfg {
            path: findings[0]
                .canonical
                .as_ref()
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf(),
            access: RootAccess::Read,
        };
        let store =
            OsUserConfigStore::new(Arc::new(OsIo) as Arc<dyn StoreIo>, home.join("config.toml"));
        let batch = store
            .prepare_batch(root_changes(&UserConfigTarget::Base, &[root]))
            .unwrap();
        let after = resolve_candidate(&layers, batch.candidate(), &workspace).unwrap();
        assert!(
            read_admitted(
                &after,
                &workspace,
                &CanonPath::new(findings[0].canonical.as_ref().unwrap())
            ),
            "after={after:?}"
        );
        let result = preview(
            &layers, &effective, &workspace, &home, None, &probes, &findings,
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.items, [0]);
        assert_eq!(result.stored_after.len(), 1);
        assert_eq!(result.stored_after[0].access, RootAccess::Read);
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            "roots=[]\n"
        );
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 1);
    }

    // Catches: root changes lift explicit denies or are proposed despite a higher-layer mask.
    #[test]
    fn setup_root_preview_refuses_denied_or_masked_reads() {
        let (_temp, workspace, home, mut layers, mut effective, probes, findings) =
            fixture("[permissions]\ndeny=['read(/**)']\n");
        assert!(
            preview(
                &layers, &effective, &workspace, &home, None, &probes, &findings
            )
            .unwrap()
            .is_none()
        );
        layers = vec![
            ConfigLayer::parse(Source::User, "roots=[]").unwrap(),
            ConfigLayer::parse(Source::System, "roots=[]").unwrap(),
        ];
        effective = resolve_selected(&layers, Trust::Untrusted, None).unwrap();
        assert!(
            preview(
                &layers, &effective, &workspace, &home, None, &probes, &findings
            )
            .unwrap()
            .is_none()
        );
    }

    // Catches: a user list replacement drops a lower-layer deny or root ceiling.
    #[test]
    fn setup_root_preview_preserves_lower_layer_denies_and_respects_ceiling() {
        let (_temp, workspace, home, mut layers, _, probes, findings) = fixture("");
        layers.push(
            ConfigLayer::parse(
                Source::System,
                "roots=[]\n[permissions]\ndeny=['read(/unrelated/**)']",
            )
            .unwrap(),
        );
        let effective = resolve_selected(&layers, Trust::Untrusted, None).unwrap();
        assert!(
            preview(
                &layers, &effective, &workspace, &home, None, &probes, &findings
            )
            .unwrap()
            .is_none()
        );
        layers.pop();
        layers.push(
            ConfigLayer::parse(
                Source::System,
                "[permissions]\ndeny=['read(/unrelated/**)']",
            )
            .unwrap(),
        );
        let effective = resolve_selected(&layers, Trust::Untrusted, None).unwrap();
        let result = preview(
            &layers, &effective, &workspace, &home, None, &probes, &findings,
        )
        .unwrap()
        .unwrap();
        let after = resolve_candidate(&layers, result.batch.candidate(), &workspace).unwrap();
        assert_eq!(after.permissions, effective.permissions);
    }
    // Catches: declined or replayed confirmation writes transaction artifacts.
    #[test]
    fn setup_decline_drops_candidate_without_config_or_lock_writes() {
        let (_temp, workspace, home, layers, effective, probes, findings) = fixture("");
        let preview = preview(
            &layers, &effective, &workspace, &home, None, &probes, &findings,
        )
        .unwrap()
        .unwrap();
        let report = confirm_apply(preview, &workspace, &home, &|question| {
            assert!(question.contains("candidate"));
            false
        })
        .unwrap();
        assert!(report.contains("declined/cancelled"));
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            "roots=[]\n"
        );
        assert_eq!(std::fs::read_dir(home).unwrap().count(), 1);
    }
}
