//! Admission of the active profile's extra roots at the process edge.

use ego_core::config::Effective;
use ego_core::permit::{CanonPath, PathResolver, ProfileRoot, admit_roots};
use ego_tools::fs::OsFs;

/// Resolve the roots the active profile declared against the real filesystem.
///
/// A configuration error never stops the run (story 966-97ad). An entry that
/// cannot be admitted is degraded to the narrower reading or skipped, and each
/// one leaves a note; see [`admit_noted`].
pub(crate) fn admit(
    effective: &Effective,
    workspace: &CanonPath,
) -> Result<Vec<ProfileRoot>, String> {
    admit_noted(effective, workspace).map(|(roots, _)| roots)
}

/// Capture independent administrator bounds without dropping restrictions.
/// Missing granting entries are skipped; missing read-only entries stay restrictive.
pub(crate) fn write_ceilings(
    effective: &Effective,
) -> (ego_core::permit::RootCeilings, Vec<String>) {
    if effective.root_ceilings.is_empty() {
        return (ego_core::permit::RootCeilings::default(), Vec::new());
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "root ceilings require HOME".to_owned())
        .and_then(|home| {
            OsFs.canonicalize(std::path::Path::new(&home))
                .map_err(|error| error.to_string())
        });
    let mut layers = Vec::new();
    let mut notes = Vec::new();
    for layer in &effective.root_ceilings {
        let mut bounds = Vec::new();
        let mut unresolved_restriction = false;
        for entry in layer {
            let admitted = home.as_ref().map_err(Clone::clone).and_then(|home| {
                admit_roots(std::slice::from_ref(entry), home, &OsFs)
                    .map_err(|error| error.to_string())
            });
            match admitted {
                Ok(mut roots) => bounds.append(&mut roots),
                Err(error) => {
                    let effect = if entry.access == ego_core::permit::RootAccess::Read {
                        let retained = home.as_ref().ok().and_then(|home| {
                            ego_core::permit::expand_root_path(&entry.path, home)
                                .ok()
                                .and_then(|expanded| through_existing_ancestor(&expanded))
                        });
                        if let Some(path) = retained {
                            bounds.push(ProfileRoot::new(path, entry.access));
                            "kept through its canonical ancestor to restrict writes"
                        } else {
                            // No canonical location proves where this restriction binds.
                            unresolved_restriction = true;
                            "this layer blocks external native writes because its restriction cannot be located"
                        }
                    } else {
                        "skipped, no write access granted"
                    };
                    notes.push(format!(
                        "administrator root ceiling {} ({}) cannot be resolved: {}; {effect}",
                        ego_core::permit::terminal_safe(&entry.path.display().to_string()),
                        entry.access.as_str(),
                        ego_core::permit::terminal_safe(&error),
                    ));
                }
            }
        }
        if unresolved_restriction {
            bounds.clear();
        }
        layers.push(bounds);
    }
    (ego_core::permit::RootCeilings::new(layers), notes)
}

/// The nearest existing ancestor of `path`, canonical, with the missing tail
/// appended. `None` when the path exists or no ancestor resolves.
fn through_existing_ancestor(path: &std::path::Path) -> Option<CanonPath> {
    if std::fs::symlink_metadata(path).is_ok() {
        return None;
    }
    let mut tail = Vec::new();
    let mut ancestor = path;
    loop {
        tail.push(ancestor.file_name()?);
        ancestor = ancestor.parent()?;
        if let Ok(canonical) = OsFs.canonicalize(ancestor) {
            let mut resolved = canonical.as_path().to_path_buf();
            resolved.extend(tail.iter().rev());
            return Some(CanonPath::new(resolved));
        }
    }
}

/// [`admit`], and one note per configured entry that was skipped or degraded
/// (path, reason, effect), for stderr and the model's first request.
///
/// Dropping an entry must never widen authority. A read root can restrict
/// writes inside the workspace or a writable root, so a missing one resolves
/// through its nearest existing canonical ancestor plus the missing tail and
/// keeps restricting. A read root that cannot be resolved for another reason
/// stays only where it restricts. Every entry that would grant authority and
/// cannot be admitted is skipped.
pub(crate) fn admit_noted(
    effective: &Effective,
    workspace: &CanonPath,
) -> Result<(Vec<ProfileRoot>, Vec<String>), String> {
    let derived = same_repository(workspace);
    if effective.roots.is_empty() && effective.root_ceilings.is_empty() {
        return Ok((derived, Vec::new()));
    }
    let (_, mut notes) = write_ceilings(effective);
    if effective.roots.is_empty() && effective.exec_paths.is_empty() {
        return Ok((derived, notes));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        "profile roots need HOME to resolve `~` and to keep it out of reach".to_owned()
    })?;
    let home = OsFs
        .canonicalize(std::path::Path::new(&home))
        .map_err(|error| format!("HOME cannot be resolved: {error}"))?;
    let mut skip = |configured: &ego_core::permit::RootCfg, reason: &str, effect: &str| {
        notes.push(format!(
            "configured root {} ({}) {reason}; {effect}",
            ego_core::permit::terminal_safe(&configured.path.display().to_string()),
            configured.access.as_str(),
        ));
    };
    let writable: Vec<std::path::PathBuf> = effective
        .roots
        .iter()
        .filter(|root| root.access == ego_core::permit::RootAccess::ReadWrite)
        .map(|root| root.path.clone())
        .collect();
    let mut roots: Vec<ProfileRoot> = Vec::new();
    for configured in &effective.roots {
        if let Some(receipt) = &configured.consent {
            let store = ego_tools::store::JsonlStore::new(
                &receipt.store,
                workspace.as_path(),
                Box::new(ego_tools::store::OsIo),
            );
            let committed = store.resume(&receipt.session).is_ok_and(|(state, _)| {
                state.permanent_root_consents.iter().any(|consent| {
                    consent.receipt == *receipt
                        && consent.root.path().as_path() == configured.path
                        && consent.root.access() == configured.access
                })
            });
            if !committed {
                skip(
                    configured,
                    "has no matching committed receipt",
                    "skipped, no access granted; remove it with ego roots rm and approve again",
                );
                continue;
            }
        }
        let admitted = match admit_roots(std::slice::from_ref(configured), &home, &OsFs) {
            Ok(mut admitted) => admitted.pop(),
            Err(ego_core::permit::RootsError::Unresolvable { reason, .. }) => {
                let read = configured.access == ego_core::permit::RootAccess::Read;
                let expanded = ego_core::permit::expand_root_path(&configured.path, &home)
                    .map_err(|error| error.to_string())?;
                if let (true, Some(resolved)) = (read, through_existing_ancestor(&expanded)) {
                    skip(
                        configured,
                        "does not exist",
                        &format!("kept as a read root at {resolved}, so it still restricts writes"),
                    );
                    Some(ProfileRoot::new(resolved, configured.access))
                } else if read
                    && expanded.is_absolute()
                    && (expanded.starts_with(workspace.as_path())
                        || writable.iter().any(|root| expanded.starts_with(root)))
                {
                    skip(
                        configured,
                        &format!("cannot be resolved: {reason}"),
                        "kept by its configured path, so it still restricts writes",
                    );
                    Some(ProfileRoot::new(
                        CanonPath::new(expanded),
                        configured.access,
                    ))
                } else {
                    skip(
                        configured,
                        &format!("cannot be resolved: {reason}"),
                        "skipped, no access granted",
                    );
                    None
                }
            }
            Err(error) => {
                skip(configured, &error.to_string(), "skipped, no access granted");
                None
            }
        };
        let Some(root) = admitted else { continue };
        let root = match effective
            .root_origins
            .iter()
            .find(|(entry, _, _)| entry == configured)
        {
            Some((_, source, ceiling)) => root.with_provenance(Some(*source), ceiling.clone()),
            None => root,
        };
        let root = if configured.consent.is_some() {
            root.with_terminal_write()
        } else {
            root
        };
        match ego_core::permit::check_root_ceilings(
            std::slice::from_ref(&root),
            &effective.root_ceilings,
            &home,
            workspace,
            &OsFs,
        ) {
            Ok(()) => roots.push(root),
            // A ceiling path that is gone cannot be compared with, so the
            // entry cannot be proved inside it. A read root that restricts
            // writes is kept anyway: dropping it would open its subtree.
            Err(ego_core::permit::RootsError::Unresolvable { .. })
                if root.access() == ego_core::permit::RootAccess::Read
                    && (root.path().is_or_under(workspace)
                        || writable
                            .iter()
                            .any(|path| root.path().as_path().starts_with(path))) =>
            {
                skip(
                    configured,
                    "could not be checked against a system ceiling that cannot be resolved",
                    "kept, because it restricts writes",
                );
                roots.push(root);
            }
            Err(error) => skip(configured, &error.to_string(), "skipped, no access granted"),
        }
    }
    for path in &effective.exec_paths {
        let extra = ProfileRoot::new(
            OsFs.canonicalize(path).map_err(|error| error.to_string())?,
            ego_core::permit::RootAccess::Read,
        );
        ego_core::permit::check_root_ceilings(
            std::slice::from_ref(&extra),
            &effective.root_ceilings,
            &home,
            workspace,
            &OsFs,
        )
        .map_err(|error| error.to_string())?;
    }
    let added: Vec<_> = derived
        .into_iter()
        .filter(|root| !roots.contains(root))
        .collect();
    roots.extend(added);
    Ok((roots, notes))
}

/// The workspace's own repository is one workspace: its other checkouts and
/// git directory get the workspace's access. Not configuration, so no ceiling
/// applies. Derived again on every call, so a worktree added mid-session joins.
pub(crate) fn same_repository(workspace: &CanonPath) -> Vec<ProfileRoot> {
    ego_tools::exec::same_repository_roots(workspace.as_path())
        .into_iter()
        .filter(|path| !path.is_or_under(workspace))
        .map(|path| {
            ProfileRoot::new(path, ego_core::permit::RootAccess::ReadWrite)
                .derived_from(ego_core::permit::DerivedRoot::SameRepository)
        })
        .collect()
}

/// Drop canonical extra roots that the host ceiling does not admit.
pub(crate) fn clamp(
    effective: &mut Effective,
    ceiling: &Effective,
    workspace: &std::path::Path,
) -> Result<(), String> {
    let home = std::env::var_os("HOME").ok_or("profile ceilings require HOME")?;
    let home = OsFs
        .canonicalize(std::path::Path::new(&home))
        .map_err(|error| error.to_string())?;
    let workspace = OsFs
        .canonicalize(workspace)
        .map_err(|error| error.to_string())?;
    let mut bounds = ceiling.roots.clone();
    let roots = admit_roots(&ceiling.roots, &home, &OsFs).map_err(|error| error.to_string())?;
    for path in &ceiling.exec_paths {
        let path = OsFs.canonicalize(path).map_err(|error| error.to_string())?;
        if !roots.iter().any(|root| path.is_or_under(root.path())) {
            bounds.push(ego_core::permit::RootCfg {
                consent: None,
                path: path.as_path().to_path_buf(),
                access: ego_core::permit::RootAccess::Read,
            });
        }
    }
    // Validate the whole ceiling first, even when the request has no roots.
    let admitted_bounds = admit_roots(&bounds, &home, &OsFs).map_err(|error| error.to_string())?;
    let ceilings = vec![bounds.clone()];
    let mut kept = Vec::new();
    let mut kept_origins = Vec::new();
    for root in &effective.roots {
        let admitted = admit_roots(std::slice::from_ref(root), &home, &OsFs)
            .map_err(|error| error.to_string())?;
        match ego_core::permit::check_root_ceilings(&admitted, &ceilings, &home, &workspace, &OsFs)
        {
            Ok(()) => {
                kept.push(root.clone());
                if let Some(origin) = effective
                    .root_origins
                    .iter()
                    .find(|(entry, _, _)| entry == root)
                {
                    kept_origins.push(origin.clone());
                }
            }
            Err(ego_core::permit::RootsError::OutsideCeiling { .. }) => {
                let read = vec![ProfileRoot::new(
                    admitted[0].path().clone(),
                    ego_core::permit::RootAccess::Read,
                )];
                if root.access == ego_core::permit::RootAccess::ReadWrite
                    && ego_core::permit::check_root_ceilings(
                        &read, &ceilings, &home, &workspace, &OsFs,
                    )
                    .is_ok()
                {
                    let narrowed = ego_core::permit::RootCfg {
                        consent: root.consent.clone(),
                        path: root.path.clone(),
                        access: ego_core::permit::RootAccess::Read,
                    };
                    if let Some((_, source, _)) = effective
                        .root_origins
                        .iter()
                        .find(|(entry, _, _)| entry == root)
                    {
                        let requested = admitted[0].path();
                        let containing = admitted_bounds
                            .iter()
                            .enumerate()
                            .filter(|(_, bound)| requested.is_or_under(bound.path()))
                            .max_by_key(|(_, bound)| bound.path().as_path().components().count());
                        let limiting = containing
                            .filter(|(_, bound)| {
                                bound.access() == ego_core::permit::RootAccess::Read
                            })
                            .or_else(|| {
                                admitted_bounds.iter().enumerate().find(|(_, bound)| {
                                    bound.access() == ego_core::permit::RootAccess::Read
                                        && bound.path().is_or_under(requested)
                                        && !bound.path().is_or_under(&workspace)
                                })
                            });
                        let ceiling_origin = limiting.and_then(|(index, _)| {
                            ceiling
                                .root_origins
                                .iter()
                                .find(|(entry, _, _)| entry == &bounds[index])
                                .map(|(_, origin, _)| {
                                    format!("{origin} config host profile ceiling")
                                })
                        });
                        kept_origins.push((narrowed.clone(), *source, ceiling_origin));
                    }
                    kept.push(narrowed);
                    effective.profile_warnings.push(format!(
                        "write access to {} is blocked by the profile; read access only",
                        root.path.display()
                    ));
                } else {
                    effective.profile_warnings.push(format!(
                        "file access to {} is blocked by the profile; path skipped",
                        root.path.display()
                    ));
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    effective.roots = kept;
    effective.root_origins = kept_origins;
    // Keep an explicit empty perimeter empty when defaults are applied later.
    effective.roots_source = Some(ego_core::config::Source::Flags);
    let mut kept = Vec::new();
    for path in &effective.exec_paths {
        let admitted = vec![ProfileRoot::new(
            OsFs.canonicalize(path).map_err(|error| error.to_string())?,
            ego_core::permit::RootAccess::Read,
        )];
        match ego_core::permit::check_root_ceilings(&admitted, &ceilings, &home, &workspace, &OsFs)
        {
            Ok(()) => kept.push(path.clone()),
            Err(ego_core::permit::RootsError::OutsideCeiling { .. }) => {
                effective.profile_warnings.push(format!(
                    "exec file access to {} is blocked by the profile; path skipped",
                    path.display()
                ))
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    effective.exec_paths = kept;
    effective
        .root_ceilings
        .extend(ceiling.root_ceilings.clone());
    effective.root_ceilings.extend(ceilings);
    Ok(())
}

/// Report confinement warnings without adding implicit filesystem authority.
pub(crate) fn defaults(effective: &mut Effective, _home: &std::path::Path) -> Vec<String> {
    let mut warnings = Vec::new();
    if effective.sandbox == Some(ego_core::permit::Confinement::Off) {
        warnings.push(
            "sandbox is off; commands can access any file your user account can access".into(),
        );
    }
    warnings
}

#[cfg(test)]
mod portable_defaults {
    use super::*;

    // Catches: canonical capture drops empty or unresolved administrator layers,
    // or treats independently declared bounds as their union.
    #[test]
    fn write_ceiling_capture_preserves_empty_layers_and_narrows_unresolved_bounds() {
        let tree = tempfile::tempdir().unwrap();
        let base = tree.path().canonicalize().unwrap();
        let mut effective =
            ego_core::config::resolve(&[], ego_core::config::Trust::Untrusted, None).unwrap();
        assert!(write_ceilings(&effective).0.is_unbounded());
        effective.root_ceilings = vec![vec![]];
        assert_eq!(
            write_ceilings(&effective).0,
            ego_core::permit::RootCeilings::new(vec![vec![]])
        );
        let bound = ego_core::permit::RootCfg {
            consent: None,
            path: base.clone(),
            access: ego_core::permit::RootAccess::ReadWrite,
        };
        effective.root_ceilings = vec![vec![bound], vec![]];
        assert_eq!(
            write_ceilings(&effective).0,
            ego_core::permit::RootCeilings::new(vec![
                vec![ProfileRoot::new(
                    CanonPath::new(&base),
                    ego_core::permit::RootAccess::ReadWrite
                )],
                vec![],
            ])
        );
        let allowed = base.join("allowed");
        std::fs::create_dir(&allowed).unwrap();
        let frozen = allowed.join("missing/readonly");
        effective.root_ceilings = vec![vec![
            ego_core::permit::RootCfg {
                consent: None,
                path: allowed.clone(),
                access: ego_core::permit::RootAccess::ReadWrite,
            },
            ego_core::permit::RootCfg {
                consent: None,
                path: base.join("missing/descendant"),
                access: ego_core::permit::RootAccess::ReadWrite,
            },
            ego_core::permit::RootCfg {
                consent: None,
                path: frozen.clone(),
                access: ego_core::permit::RootAccess::Read,
            },
        ]];
        let (ceilings, notes) = write_ceilings(&effective);
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("skipped, no write access granted"));
        assert!(notes[1].contains("kept through its canonical ancestor"));
        let workspace = CanonPath::new(base.join("workspace"));
        let confinement = ego_core::permit::RunConfinement::new(
            ego_core::permit::Confinement::Off,
            false,
            workspace.clone(),
        )
        .unwrap();
        let policy = ego_core::permit::PolicyEngine::new(
            workspace.clone(),
            ego_core::config::Mode::Yolo,
            ego_core::config::Ruleset::default(),
        )
        .with_read_perimeter(ego_core::permit::ReadPerimeter::from_confinement(
            &confinement,
        ))
        .with_root_ceilings(ceilings);
        for (path, expected) in [
            (allowed.join("note"), true),
            (frozen.join("note"), false),
            (base.join("missing/descendant/note"), false),
            (base.join("other"), false),
        ] {
            let admitted = match policy.classify(ego_core::permit::PreparedCall::new(
                "write",
                ego_core::permit::Op::Write {
                    path: CanonPath::new(&path),
                    change: ego_core::permit::Change::Whole(vec![]),
                },
            )) {
                ego_core::permit::Classification::Settled(result) => result.is_ok(),
                ego_core::permit::Classification::Ask(pending) => matches!(
                    futures::executor::block_on(policy.settle(pending)),
                    ego_core::permit::Resolution::Settled(Ok(_)),
                ),
            };
            assert_eq!(admitted, expected, "{}", path.display());
        }
        let (_, admission_notes) = admit_noted(&effective, &workspace).unwrap();
        assert_eq!(admission_notes, notes);

        // Catches: a malformed relative restriction is anchored to the process cwd.
        effective.root_ceilings[0].push(ego_core::permit::RootCfg {
            consent: None,
            path: std::path::PathBuf::from("./missing-readonly"),
            access: ego_core::permit::RootAccess::Read,
        });
        let (ceilings, notes) = write_ceilings(&effective);
        assert_eq!(ceilings, ego_core::permit::RootCeilings::new(vec![vec![]]));
        assert_eq!(notes.len(), 3);
        assert!(notes[2].contains("restriction cannot be located"));
    }

    // Catches: the list-level user source is wrongly attributed to a project ceiling root.
    #[test]
    fn refusal_names_root_and_source_for_mixed_source_profile() {
        use ego_core::config::{ConfigLayer, Mode, Ruleset, Source, Trust, resolve};
        use ego_core::permit::{Change, Classification, Op, PolicyEngine, PreparedCall};
        let tree = tempfile::tempdir().unwrap();
        let ws = tree.path().join("workspace");
        let user = tree.path().join("user-root");
        let project = tree.path().join("project-root");
        for path in [&ws, &user, &project] {
            std::fs::create_dir(path).unwrap();
        }
        let ws = CanonPath::new(ws.canonicalize().unwrap());
        let user = user.canonicalize().unwrap();
        let project = project.canonicalize().unwrap();
        let user_layer = ConfigLayer::parse(
            Source::User,
            &format!("roots=[{{path='{}',access='read'}}]", user.display()),
        )
        .unwrap();
        let project_layer = ConfigLayer::parse(
            Source::Project,
            &format!(
                "[grants]\nroots=[{{path='{}',access='read'}}]",
                project.display()
            ),
        )
        .unwrap();
        let ceiling = resolve(&[user_layer, project_layer], Trust::Trusted, None).unwrap();
        let request = ConfigLayer::parse(
            Source::User,
            &format!(
                "roots=[{{path='{}',access='read-write'}}]",
                project.display()
            ),
        )
        .unwrap();
        let mut effective = resolve(&[request], Trust::Trusted, None).unwrap();
        clamp(&mut effective, &ceiling, ws.as_path()).unwrap();
        let roots = admit(&effective, &ws).unwrap();
        let engine =
            PolicyEngine::new(ws, Mode::Yolo, Ruleset::default()).with_profile_roots(roots);
        let call = PreparedCall::new(
            "edit",
            Op::Write {
                path: CanonPath::new(project.join("file")),
                change: Change::Whole(vec![]),
            },
        );
        let Classification::Settled(Err(denied)) = engine.classify(call) else {
            panic!("write must be refused")
        };
        assert_eq!(denied.to_string(), "blocked: root ceiling");
        let ego_core::permit::PolicyDenied::ReadOnlyRoot { root, .. } = denied else {
            panic!("expected a root ceiling refusal")
        };
        assert_eq!(root.source(), Some(Source::User));
        assert_eq!(
            root.ceiling_source(),
            Some("project config host profile ceiling")
        );
    }

    // Story 966-97ad. Catches: skipping a missing read root opens its path to
    // writes through the writable root that contains it.
    #[test]
    fn a_missing_read_root_under_a_writable_root_still_restricts_writes() {
        use ego_core::config::{ConfigLayer, Source, Trust, resolve};
        let tree = tempfile::tempdir().unwrap();
        let ws = tree.path().join("workspace");
        let rw = tree.path().join("rw");
        std::fs::create_dir(&ws).unwrap();
        std::fs::create_dir(&rw).unwrap();
        let ws = CanonPath::new(ws.canonicalize().unwrap());
        let rw = rw.canonicalize().unwrap();
        let missing = rw.join("gone/deeper");
        let layer = ConfigLayer::parse(
            Source::User,
            &format!(
                "roots=[{{path='{}',access='read-write'}},{{path='{}',access='read'}},{{path='{}/old.bak',access='read-write'}}]",
                rw.display(),
                missing.display(),
                tree.path().join("absent").display()
            ),
        )
        .unwrap();
        let effective = resolve(&[layer], Trust::Trusted, None).unwrap();
        let (roots, notes) = admit_noted(&effective, &ws).unwrap();
        let read = roots
            .iter()
            .find(|root| root.access() == ego_core::permit::RootAccess::Read)
            .expect("the missing read root is kept");
        assert_eq!(read.path().as_path(), missing);
        assert!(read.restricts_writes(&ws, &roots));
        assert_eq!(roots.len(), 2, "the missing read-write root grants nothing");
        assert_eq!(notes.len(), 2, "{notes:?}");
    }

    // Story 966-97ad. Catches: a ceiling whose own path is gone makes every
    // configured entry fail, so a read root that restricts writes inside the
    // workspace is dropped and its subtree opens to writes.
    #[test]
    fn a_missing_ceiling_path_does_not_drop_a_read_root_that_restricts_writes() {
        use ego_core::config::{ConfigLayer, Source, Trust, resolve};
        let tree = tempfile::tempdir().unwrap();
        let ws = tree.path().join("workspace");
        std::fs::create_dir_all(ws.join("secret")).unwrap();
        let ws = CanonPath::new(ws.canonicalize().unwrap());
        let layer = ConfigLayer::parse(
            Source::User,
            &format!(
                "roots=[{{path='{}/secret',access='read'}}]",
                ws.as_path().display()
            ),
        )
        .unwrap();
        let mut effective = resolve(&[layer], Trust::Trusted, None).unwrap();
        effective
            .root_ceilings
            .push(vec![ego_core::permit::RootCfg {
                consent: None,
                path: tree.path().join("deleted-ceiling"),
                access: ego_core::permit::RootAccess::ReadWrite,
            }]);
        let (roots, _notes) = admit_noted(&effective, &ws).unwrap();
        assert!(
            roots
                .iter()
                .any(|root| root.path().as_path() == ws.as_path().join("secret")
                    && root.restricts_writes(&ws, &roots)),
            "the read root under the workspace was dropped: {roots:?}"
        );
    }

    // Catches: the main checkout of the workspace repository counts as outside
    // the workspace, or a sibling repository joins it by parent directory.
    #[test]
    fn linked_worktree_reaches_the_main_checkout_and_no_sibling_repository() {
        use ego_core::config::{Mode, Ruleset};
        use ego_core::permit::{
            Change, Classification, Op, PolicyDenied, PolicyEngine, PreparedCall,
        };
        let tree = tempfile::tempdir().unwrap();
        let base = tree.path().canonicalize().unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(status.status.success());
        };
        for name in ["main", "sibling"] {
            std::fs::create_dir(base.join(name)).unwrap();
            git(&base.join(name), &["init", "-q"]);
        }
        git(
            &base.join("main"),
            &["commit", "-q", "--allow-empty", "-m", "x"],
        );
        git(
            &base.join("main"),
            &["worktree", "add", "-q", "../wt", "-b", "wt"],
        );
        let ws = CanonPath::new(base.join("wt"));
        let effective =
            ego_core::config::resolve(&[], ego_core::config::Trust::Untrusted, None).unwrap();
        let roots = admit(&effective, &ws).unwrap();
        assert!(roots.iter().all(|root| {
            root.derived() == Some(ego_core::permit::DerivedRoot::SameRepository)
                && root.access() == ego_core::permit::RootAccess::ReadWrite
        }));
        let engine =
            PolicyEngine::new(ws, Mode::Yolo, Ruleset::default()).with_profile_roots(roots);
        let write = |path: std::path::PathBuf| {
            engine.classify(PreparedCall::new(
                "write",
                Op::Write {
                    path: CanonPath::new(path),
                    change: Change::Whole(vec![]),
                },
            ))
        };
        assert!(
            !matches!(
                write(base.join("main/stories-x.md")),
                Classification::Settled(Err(denied)) if matches!(denied, PolicyDenied::OutsideWorkspace { .. })
            ),
            "main checkout must be inside the workspace"
        );
        assert!(matches!(
            write(base.join("sibling/x.md")),
            Classification::Settled(Err(denied)) if matches!(denied, PolicyDenied::OutsideWorkspace { .. })
        ));
    }

    // Catches: a home folder name grants read-write reach without user configuration.
    #[test]
    fn portable_defaults_do_not_grant_home_layout_roots() {
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir(tree.path().join("Gits")).unwrap();
        let mut effective =
            ego_core::config::resolve(&[], ego_core::config::Trust::Untrusted, None).unwrap();
        defaults(&mut effective, tree.path());
        assert!(effective.roots.is_empty());
    }
}
