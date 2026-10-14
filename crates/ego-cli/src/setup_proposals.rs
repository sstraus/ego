//! Deterministic read/write-root proposals, resolved through the real configuration layers.

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
    pub writes: Vec<bool>,
    pub executions: Vec<bool>,
    pub exec_before: Vec<std::path::PathBuf>,
    pub exec_after: Vec<std::path::PathBuf>,
    pub judge: Option<String>,
    pub target: UserConfigTarget,
    pub layers: Vec<Sourced<ConfigLayer>>,
    pub identities: Vec<String>,
    pub config_identity: String,
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

#[cfg(test)]
fn read_admitted(effective: &Effective, workspace: &CanonPath, path: &CanonPath) -> bool {
    operation_admitted(effective, workspace, path, false)
}

fn operation_admitted(
    effective: &Effective,
    workspace: &CanonPath,
    path: &CanonPath,
    write: bool,
) -> bool {
    if write && effective.sandbox != Some(ego_core::permit::Confinement::Workspace) {
        return false;
    }
    if write && ego_core::config::Ruleset::is_credential_path(path) {
        return false;
    }
    let Ok(roots) = crate::run::profile_roots::admit(effective, workspace) else {
        return false;
    };
    let policy = PolicyEngine::new(
        workspace.clone(),
        effective.mode.unwrap_or(ego_core::config::Mode::Default),
        effective.permissions.clone(),
    )
    .with_profile_roots(roots);
    let op = if write {
        Op::Write {
            path: path.clone(),
            change: ego_core::permit::Change::Whole(Vec::new()),
        }
    } else {
        Op::Read { path: path.clone() }
    };
    match policy.classify(PreparedCall::new(if write { "write" } else { "read" }, op)) {
        Classification::Settled(Ok(_)) => true,
        Classification::Ask(pending) => matches!(
            futures::executor::block_on(policy.settle(pending)),
            Resolution::Settled(Ok(_))
        ),
        _ => false,
    }
}

fn exec_admitted(effective: &Effective, workspace: &CanonPath, path: &CanonPath) -> bool {
    use ego_core::permit::PathResolver;
    let Ok(policy) = ego_tools::exec::admit_confinement(
        effective.sandbox.unwrap_or_default(),
        effective.network == ego_core::config::Network::Off,
        workspace.clone(),
    ) else {
        return false;
    };
    let Ok(paths) = effective
        .exec_paths
        .iter()
        .map(|path| ego_tools::fs::OsFs.canonicalize(path))
        .collect::<Result<Vec<_>, _>>()
    else {
        return false;
    };
    #[cfg(target_os = "linux")]
    if effective.roots.iter().any(|root| {
        root.access == RootAccess::Read
            && paths
                .iter()
                .any(|exec| root.path.starts_with(exec.as_path()))
    }) {
        return false;
    }
    ego_tools::exec::setup_exec_reachable(&policy.with_exec_paths(paths), path).unwrap_or(false)
}

fn batch_changes(
    target: &UserConfigTarget,
    roots: &[RootCfg],
    exec: &[std::path::PathBuf],
    change_exec: bool,
) -> Vec<UserConfigMutation> {
    let mut changes = root_changes(target, roots);
    if change_exec {
        changes.push(UserConfigMutation::ManagedSet {
            target: target.clone(),
            key: ManagedConfigKey::ExecPaths,
            value: ManagedConfigValue::ExecPaths(exec.to_vec()),
        });
    }
    changes
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
/// Workspace writes are policy-only; other backend/consumer gaps remain explicit.
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
    let exec_before = snapshot
        .values
        .iter()
        .find_map(|(key, value)| match (key, value) {
            (ManagedConfigKey::ExecPaths, Some(ManagedConfigValue::ExecPaths(paths))) => {
                Some(paths.clone())
            }
            _ => None,
        })
        .unwrap_or_default();
    let mut exec_after = exec_before.clone();
    for path in &effective.exec_paths {
        if !exec_after.contains(path) {
            exec_after.push(path.clone());
        }
    }
    let mut roots = stored.clone();
    // Roots replace a layer list. Preserve the already effective lower-layer reach.
    for root in &effective.roots {
        if !roots.contains(root) {
            roots.push(root.clone());
        }
    }
    let mut items = Vec::new();
    let mut paths = Vec::new();
    let mut writes = Vec::new();
    let mut executions = Vec::new();
    let mut change_exec = false;
    for finding in findings {
        let execution = matches!(probes.get(finding.id), Some(Probe::Tool { .. }));
        if !matches!(finding.status, Status::Blocked)
            && !(execution && matches!(finding.status, Status::Unknown))
        {
            continue;
        }
        if !matches!(
            probes.get(finding.id),
            Some(
                Probe::Read { .. }
                    | Probe::Instructions { .. }
                    | Probe::Git { .. }
                    | Probe::Write { .. }
                    | Probe::Tool { .. }
            )
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
        if execution {
            if exec_admitted(effective, workspace, &CanonPath::new(path)) {
                continue;
            }
            let metadata = match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => metadata,
                _ => continue,
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = metadata;
                continue;
            }
        }
        let write = matches!(probes.get(finding.id), Some(Probe::Write { .. }));
        if write
            && (effective.sandbox != Some(ego_core::permit::Confinement::Workspace)
                || !ego_tools::exec::setup_write_root_safe(directory, path).unwrap_or(false))
        {
            continue;
        }
        let root = RootCfg {
            path: directory.to_path_buf(),
            access: if write {
                RootAccess::ReadWrite
            } else {
                RootAccess::Read
            },
        };
        let mut proposed = roots.clone();
        let mut proposed_exec = exec_after.clone();
        if execution && !proposed_exec.contains(&directory.to_path_buf()) {
            proposed_exec.push(directory.to_path_buf());
        }
        if !execution {
            if let Some(existing) = proposed
                .iter_mut()
                .find(|existing| existing.path == root.path)
            {
                if write {
                    existing.access = RootAccess::ReadWrite;
                }
            } else {
                proposed.push(root);
            }
        }
        let batch = store
            .prepare_batch(batch_changes(
                &target,
                &proposed,
                &proposed_exec,
                change_exec || execution,
            ))
            .map_err(|error| error.to_string())?;
        let Ok(after) = resolve_candidate(layers, batch.candidate(), workspace) else {
            continue;
        };
        // Higher layers, explicit deny, floors and ceilings cannot be lifted.
        if after.permissions != effective.permissions
            || after.mode != effective.mode
            || after.sandbox != effective.sandbox
            || after.network != effective.network
            || (!execution && !change_exec && after.exec_paths != effective.exec_paths)
            || !effective
                .exec_paths
                .iter()
                .all(|path| after.exec_paths.contains(path))
        {
            continue;
        }
        if !effective.roots.iter().all(|root| {
            after.roots.iter().any(|new| {
                new.path == root.path
                    && (new.access == root.access || new.access == RootAccess::ReadWrite)
            })
        }) {
            continue;
        }
        if !(if execution {
            exec_admitted(&after, workspace, &CanonPath::new(path))
        } else {
            operation_admitted(&after, workspace, &CanonPath::new(path), write)
        }) {
            continue;
        }
        roots = proposed;
        exec_after = proposed_exec;
        change_exec |= execution;
        items.push(finding.id);
        paths.push(if execution {
            path.clone()
        } else {
            finding.requested.clone()
        });
        writes.push(write);
        executions.push(execution);
    }
    if items.is_empty() {
        return Ok(None);
    }
    let batch = store
        .prepare_batch(batch_changes(&target, &roots, &exec_after, change_exec))
        .map_err(|error| error.to_string())?;
    let after = resolve_candidate(layers, batch.candidate(), workspace)?;
    if paths
        .iter()
        .zip(&writes)
        .zip(&executions)
        .any(|((path, write), execution)| {
            !(if *execution {
                exec_admitted(&after, workspace, &CanonPath::new(path))
            } else {
                operation_admitted(&after, workspace, &CanonPath::new(path), *write)
            })
        })
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
        writes,
        executions,
        exec_before,
        exec_after,
        judge: None,
        target,
        layers: layers.to_vec(),
        identities,
        config_identity: config_identity(home)?,
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
        "Setup {setup_id}: items {:?}, target {:?}\nDirectory exposure includes siblings/descendants. Stored roots {:?} -> {:?}; exec directories {:?} -> {:?} (directory reach, not single executable)\nJudge {:?}: future inference may spend money; this does not grant an action or lift network/floors\nPaths {:?}\nBase {base}, candidate {candidate}. Apply? [y/N] ",
        preview.items,
        preview.target,
        preview.stored_before,
        preview.stored_after,
        preview.exec_before,
        preview.exec_after,
        preview.judge,
        preview.paths
    );
    if !confirm(question) {
        return Ok("Setup declined/cancelled; no configuration writes.\n".into());
    }
    let check = || {
        if config_identity(home).map_err(UserConfigError::new)? != preview.config_identity {
            return Err(UserConfigError::new(
                "setup configuration path changed; confirm a fresh preview",
            ));
        }
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
    let writes = preview.writes.clone();
    let executions = preview.executions.clone();
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
    for ((path, write), execution) in paths.into_iter().zip(writes).zip(executions) {
        let status = if path_identity(&path).is_err() {
            "unknown"
        } else if execution && exec_admitted(&effective, workspace, &CanonPath::new(&path)) {
            "fixed executable reach (policy-only; action/dependencies unproven)"
        } else if !execution
            && operation_admitted(&effective, workspace, &CanonPath::new(&path), write)
        {
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

fn config_identity(home: &Path) -> Result<String, String> {
    let path = home.join("config.toml");
    let canonical_home = std::fs::canonicalize(home).map_err(|error| error.to_string())?;
    let target = if path.exists() {
        path_identity(&path)?
    } else {
        "absent".into()
    };
    Ok(format!("{}:{target}", canonical_home.display()))
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
    // Catches: the mapper omits effective Workspace writes or offers them under Ro.
    #[test]
    fn setup_write_preview_requires_workspace_and_preserves_unrelated_policy() {
        let (_temp, workspace, home, layers, effective, mut probes, findings) = fixture(
            "sandbox='workspace'\nmode='edits'\n[permissions]\ndeny=['write(/unrelated/**)']\n",
        );
        probes[0] = Probe::Write {
            path: findings[0].requested.clone(),
            reason: "expected output".into(),
        };
        let proposed = preview(
            &layers, &effective, &workspace, &home, None, &probes, &findings,
        )
        .unwrap()
        .expect("effective Workspace write proposal is missing");
        assert_eq!(proposed.stored_after[0].access, RootAccess::ReadWrite);
        let after = resolve_candidate(&layers, proposed.batch.candidate(), &workspace).unwrap();
        assert_eq!(after.permissions, effective.permissions);
        assert_eq!(after.sandbox, effective.sandbox);
        assert_eq!(
            std::fs::read_to_string(&findings[0].requested).unwrap(),
            "task evidence"
        );
        let default = ConfigLayer::parse(Source::User, "roots=[]\nsandbox='workspace'").unwrap();
        let default_effective =
            resolve_selected(std::slice::from_ref(&default), Trust::Untrusted, None).unwrap();
        assert!(
            preview(
                &[default],
                &default_effective,
                &workspace,
                &home,
                None,
                &probes,
                &findings
            )
            .unwrap()
            .is_none(),
            "reach alone cannot resolve a write approval"
        );
        let ro = ConfigLayer::parse(Source::User, "roots=[]\nsandbox='ro'").unwrap();
        let ro_effective =
            resolve_selected(std::slice::from_ref(&ro), Trust::Untrusted, None).unwrap();
        assert!(
            preview(
                &[ro],
                &ro_effective,
                &workspace,
                &home,
                None,
                &probes,
                &findings
            )
            .unwrap()
            .is_none()
        );
    }

    // Catches: strict tool reach gets no directory proposal or loses stored exec paths.
    #[test]
    fn setup_exec_preview_preserves_paths_and_respects_mask() {
        let (_temp, workspace, home, layers, effective, mut probes, mut findings) =
            fixture("sandbox='ro'\n");
        let path = findings[0].requested.clone();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        probes[0] = Probe::Tool {
            name: "fixture-tool".into(),
            reason: "expected tool".into(),
        };
        findings[0].status = Status::Unknown;
        let result = preview(
            &layers, &effective, &workspace, &home, None, &probes, &findings,
        )
        .unwrap()
        .expect("strict executable reach proposal missing");
        let after = resolve_candidate(&layers, result.batch.candidate(), &workspace).unwrap();
        assert_eq!(after.exec_paths, vec![path.parent().unwrap().to_path_buf()]);
        let existing = workspace.as_path().join("existing-bin");
        std::fs::create_dir(&existing).unwrap();
        let preserved = ConfigLayer::parse(
            Source::User,
            &format!(
                "roots=[]\nsandbox='ro'\nexec_paths=[{:?}]",
                existing.display().to_string()
            ),
        )
        .unwrap();
        let preserved_layers = vec![preserved];
        let preserved_effective =
            resolve_selected(&preserved_layers, Trust::Untrusted, None).unwrap();
        let preserved_preview = preview(
            &preserved_layers,
            &preserved_effective,
            &workspace,
            &home,
            None,
            &probes,
            &findings,
        )
        .unwrap()
        .unwrap();
        let preserved_after = resolve_candidate(
            &preserved_layers,
            preserved_preview.batch.candidate(),
            &workspace,
        )
        .unwrap();
        assert!(preserved_after.exec_paths.contains(&existing));
        assert_eq!(after.permissions, effective.permissions);
        assert_eq!(after.sandbox, effective.sandbox);
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 1);
        let mut masked = layers;
        masked.push(ConfigLayer::parse(Source::Flags, "exec_paths=[]").unwrap());
        let masked_effective = resolve_selected(&masked, Trust::Untrusted, None).unwrap();
        assert!(
            preview(
                &masked,
                &masked_effective,
                &workspace,
                &home,
                None,
                &probes,
                &findings
            )
            .unwrap()
            .is_none()
        );
    }

    // Catches: executable reach already supplied by workspace/runtime is persisted again.
    #[test]
    fn setup_exec_preview_does_not_copy_runtime_or_workspace_reach() {
        let (_temp, workspace, home, layers, effective, mut probes, mut findings) =
            fixture("sandbox='workspace'\n");
        probes[0] = Probe::Tool {
            name: "fixture-tool".into(),
            reason: "expected tool".into(),
        };
        findings[0].status = Status::Unknown;
        let path = workspace.as_path().join("program");
        std::fs::write(&path, "fixture").unwrap();
        findings[0].canonical = Some(path);
        assert!(
            preview(
                &layers, &effective, &workspace, &home, None, &probes, &findings
            )
            .unwrap()
            .is_none()
        );
        let runtime = std::fs::canonicalize("/bin/sh").unwrap();
        findings[0].canonical = Some(runtime);
        assert!(
            preview(
                &layers, &effective, &workspace, &home, None, &probes, &findings
            )
            .unwrap()
            .is_none()
        );
    }

    // Catches: an already-settled Auto write is mistaken for a missing judge.
    #[test]
    fn setup_auto_write_needs_no_gratuitous_judge_or_allow_rule() {
        let (_temp, workspace, _home, layers, _, _, _) =
            fixture("sandbox='workspace'\nmode='auto'\n");
        let effective = resolve_selected(&layers, Trust::Untrusted, None).unwrap();
        assert!(operation_admitted(
            &effective,
            &workspace,
            &CanonPath::new(workspace.as_path().join("output")),
            true
        ));
        for mode in [
            ego_core::config::Mode::Plan,
            ego_core::config::Mode::Default,
            ego_core::config::Mode::Edits,
            ego_core::config::Mode::Auto,
            ego_core::config::Mode::Yolo,
        ] {
            let mut policy = effective.clone();
            policy.mode = Some(mode);
            assert!(operation_admitted(
                &policy,
                &workspace,
                &CanonPath::new(workspace.as_path().join("input")),
                false
            ));
        }
    }

    // Catches: a write root bypasses explicit denies or exposes instruction controls.
    #[test]
    fn setup_write_preview_refuses_denies_and_control_targets() {
        for extra in [
            "sandbox='workspace'\n[permissions]\ndeny=['write(/**)']",
            "sandbox='workspace'",
        ] {
            let (_temp, workspace, home, layers, effective, mut probes, mut findings) =
                fixture(extra);
            if !extra.contains("deny") {
                let path = findings[0].requested.parent().unwrap().join("AGENTS.md");
                std::fs::write(&path, "control").unwrap();
                findings[0].requested = path.clone();
                findings[0].canonical = Some(path);
            }
            probes[0] = Probe::Write {
                path: findings[0].requested.clone(),
                reason: "expected output".into(),
            };
            assert!(
                preview(
                    &layers, &effective, &workspace, &home, None, &probes, &findings
                )
                .unwrap()
                .is_none()
            );
            assert_eq!(std::fs::read_dir(&home).unwrap().count(), 1);
        }
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
    // Catches: a confirmed diff survives replacement of the bound filesystem item.
    #[test]
    fn setup_confirmed_path_change_invalidates_publication() {
        let (_temp, workspace, home, layers, effective, probes, findings) = fixture("");
        let preview = preview(
            &layers, &effective, &workspace, &home, None, &probes, &findings,
        )
        .unwrap()
        .unwrap();
        let path = findings[0].requested.clone();
        let result = confirm_apply(preview, &workspace, &home, &|_| {
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, "changed identity and bytes").unwrap();
            true
        });
        assert!(result.unwrap_err().to_string().contains("path changed"));
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            "roots=[]\n"
        );
    }

    // Catches: an accepted batch mutates captured run policy rather than fresh admission only.
    #[test]
    fn setup_confirmed_publication_reprobes_without_widening_captured_policy() {
        let (_temp, workspace, home, layers, effective, probes, findings) = fixture("");
        let policy = PolicyEngine::new(
            workspace.clone(),
            ego_core::config::Mode::Default,
            effective.permissions.clone(),
        );
        let preview = preview(
            &layers, &effective, &workspace, &home, None, &probes, &findings,
        )
        .unwrap()
        .unwrap();
        let report = confirm_apply(preview, &workspace, &home, &|_| true).unwrap();
        assert!(report.contains("fixed (policy-only)"), "{report}");
        assert!(matches!(
            policy.classify(PreparedCall::new(
                "read",
                Op::Read {
                    path: CanonPath::new(&findings[0].requested)
                }
            )),
            Classification::Settled(Err(_))
        ));
        assert!(
            std::fs::read_to_string(home.join("config.toml"))
                .unwrap()
                .contains("read")
        );
    }
}
