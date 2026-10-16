//! Resolve configuration, materials, models, and run-local inputs.

use super::*;

pub(super) fn with_permission_analysis<'session: 'turn, 'turn>(
    loop_: TurnLoop<'session, 'turn>,
    trust: Trust,
) -> TurnLoop<'session, 'turn> {
    match crate::config::ego_home() {
        Some(home) => loop_.with_permission_analysis_cache(
            Arc::new(ego_tools::permission_analysis_cache::FilePermissionAnalysisCache::new(&home)),
            trust,
        ),
        None => loop_,
    }
}

pub(super) fn deadline(spec: &str) -> Result<Duration, UsageError> {
    crate::cli::duration_seconds(spec).map(Duration::from_secs)
}

/// The workspace this run is rooted at.
///
/// `-C DIR` is resolved once and propagated explicitly. The process working
/// directory is never mutated, so concurrent rooted adapters cannot retarget
/// one another.
pub(crate) fn workspace(directory: Option<&PathBuf>) -> Result<PathBuf, UsageError> {
    let directory = match directory {
        Some(directory) => directory.clone(),
        None => std::env::current_dir().map_err(|err| {
            UsageError(format!(
                "the working directory could not be resolved: {err}"
            ))
        })?,
    };
    let canonical = std::fs::canonicalize(&directory).map_err(|err| {
        UsageError(format!(
            "the working directory {} could not be resolved: {err}",
            directory.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(UsageError(format!(
            "the working directory {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

pub(crate) fn rooted_path(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

/// `--instructions FILE`, resolved against this workspace.
///
/// Two things happen here and neither is cosmetic.
///
/// **The directory is resolved and the file name deliberately is not.**
/// `Selection::Explicit` reaches `config::plan` verbatim, and that module
/// states its inputs must already be canonical — `root` and `cwd` are, because
/// [`workspace`] canonicalises them, and this path was not. A relative
/// `AGENTS.md` therefore reached `scope_of` with an empty parent and published
/// `scope: ""`, while an absolute one published the host's own directory into
/// a stream `InstructionDescriptor`'s doc says a CI job archives. `"."` is the
/// documented answer for both. Resolving only the parent leaves the final
/// component untouched, so a symlinked file still arrives at the loader as a
/// symlink and is still refused by name instead of quietly followed.
///
/// **A file that is not there refuses the run.** `CLI.md` §4.4: invalid config
/// is never skipped silently. The operator named this file; proceeding without
/// it ignores an instruction that was given, which is the silently vacuous
/// flag `--budget` was fixed for one flag over — and `--system @file` already
/// exits 2 for the same mistake. `symlink_metadata` rather than `exists`,
/// because a *dangling* symlink must reach the loader and be refused as the
/// symlink it is, not reported here as an absence.
pub(super) fn explicit(path: &Path, cwd: &Path) -> Result<PathBuf, UsageError> {
    let refuse =
        |err: std::io::Error| UsageError(format!("--instructions {}: {err}", path.display()));
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let name = absolute.file_name().ok_or_else(|| {
        UsageError(format!(
            "--instructions {}: that is a directory, not an instruction file",
            path.display()
        ))
    })?;
    let resolved = std::fs::canonicalize(absolute.parent().unwrap_or(cwd))
        .map_err(refuse)?
        .join(name);
    std::fs::symlink_metadata(&resolved).map_err(refuse)?;
    Ok(resolved)
}

pub(super) fn rooted_at_file(value: &str, cwd: &Path) -> Result<String, UsageError> {
    let Some(path) = value.strip_prefix('@') else {
        return prompt::at_file(value);
    };
    let path = Path::new(path);
    let rooted = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    prompt::at_file(&format!("@{}", rooted.display()))
}

/// The instructions and system prompt this run generates under.
///
/// **`--bare`, and a headless run in an untrusted workspace, call
/// `InstructionIo` zero times.** Not "read nothing" — make no call at all, so
/// nothing is stat'd, opened or read, and there is nothing for a dialog to be
/// raised about. That is the mechanism behind §4.4, and it is
/// `Selection::Disabled` doing it, one layer down.
pub(super) fn instructions(
    effective: &Effective,
    args: &Args,
    root: &Path,
    cwd: &Path,
    diagnostics: &dyn Diagnostics,
) -> Result<Option<SystemPrompt>, UsageError> {
    ego_core::config::validate_prompt_options(args.bare, args.instructions.is_some())
        .map_err(|message| UsageError(message.to_owned()))?;
    let session = args.session_prompt.as_ref();
    let replacing =
        session.filter(|prompt| prompt.mode == ego_core::config::SystemPromptMode::Replace);
    let replacement = replacing
        .map(|prompt| &prompt.text)
        .or(args.system.as_ref());
    let mut assembled = if let Some(text) = replacement {
        Some(SystemPrompt::verbatim(rooted_at_file(text, cwd)?))
    } else {
        let named = match &args.instructions {
            Some(path) => Some(explicit(path, cwd)?),
            None => None,
        };
        let selection = Selection::of(effective.posture, named.clone());
        let discovery_root = if matches!(selection, Selection::Disabled) {
            root.to_path_buf()
        } else if let Some(home) = std::env::var_os("HOME") {
            crate::trust::instruction_discovery_root(root, &PathBuf::from(home)).map_err(
                |error| {
                    UsageError(format!(
                        "instruction discovery root could not be resolved: {error}"
                    ))
                },
            )?
        } else {
            root.to_path_buf()
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut set = ego_core::config::load_with_home(
            &OsInstructionIo,
            &discovery_root,
            cwd,
            &selection,
            Limits::default(),
            home.as_deref(),
        )
        .map_err(|err| UsageError(err.to_string()))?;

        // Consent covers the exact bytes staged above, not a later path read.
        let surface = if matches!(selection, Selection::Disabled) {
            ego_core::config::ExecutableSurface::default()
        } else {
            crate::trust::executable_surface_selected(root, named.as_deref())
        };
        let approved = !matches!(selection, Selection::Disabled)
            && crate::trust::remembered_decision(
                root,
                crate::config::ego_home().as_deref(),
                &surface,
            ) == Some(ego_core::config::Trust::Trusted);
        let scope = match &selection {
            Selection::Explicit(path) => path.parent().unwrap_or(&discovery_root),
            _ => &discovery_root,
        };
        let mut rejected = Vec::new();
        set.loaded.retain(|file| {
            let allowed = file.path.starts_with(scope)
                || (approved
                    && surface
                        .external_instructions
                        .get(&file.path.display().to_string())
                        == Some(&file.digest));
            if !allowed {
                rejected.push(ego_core::config::Skipped {
                    path: file.path.clone(),
                    reason: ego_core::config::Refusal::ExternalApprovalRequired,
                });
            }
            allowed
        });
        set.skipped.extend(rejected);

        for skipped in &set.skipped {
            diagnostics.note(&format!(
                "{} was not used: {}",
                skipped.path.display(),
                skipped.reason
            ));
        }

        system_prompt(&set, root, &selection)
    };
    let launch_append = args.append_system.as_ref().filter(|_| replacing.is_none());
    let session_append = session
        .filter(|prompt| prompt.mode == ego_core::config::SystemPromptMode::Append)
        .map(|prompt| &prompt.text);
    for extra in [launch_append, session_append].into_iter().flatten() {
        let extra = rooted_at_file(extra, cwd)?;
        assembled = Some(match assembled {
            Some(mut prompt) => {
                prompt.text.push_str("\n\n");
                prompt.text.push_str(&extra);
                prompt
            }
            None => SystemPrompt::verbatim(extra),
        });
    }
    Ok(assembled)
}

/// The tools this run offers the model.
///
/// `--tools` names a subset and `--no-tools` names the empty one; a name that
/// is not a builtin is refused rather than ignored, because a run that
/// silently offered fewer tools than it was told to is a run whose failure has
/// no visible cause.
#[cfg(test)]
pub(super) fn tools(
    selected: Option<&Vec<String>>,
    skills: Option<Arc<ego_core::skill::SkillCatalogue>>,
    profile: Option<&str>,
    history: Option<Arc<dyn ego_core::history::SessionHistory>>,
    environment: ego_core::permit::EnvPolicy,
) -> Result<Vec<RegisteredTool>, UsageError> {
    tools_with_mode(
        selected,
        skills,
        profile,
        history,
        environment,
        ego_core::orchestrator::Mode::Observe,
        None,
    )
}

pub(super) fn orchestration_mode(effective: &Effective) -> ego_core::orchestrator::Mode {
    if effective
        .active_profile
        .as_ref()
        .is_some_and(|p| p.name == "coordinator")
    {
        effective.orchestration
    } else {
        ego_core::orchestrator::Mode::Observe
    }
}

pub(super) fn tools_with_mode(
    selected: Option<&Vec<String>>,
    skills: Option<Arc<ego_core::skill::SkillCatalogue>>,
    profile: Option<&str>,
    history: Option<Arc<dyn ego_core::history::SessionHistory>>,
    environment: ego_core::permit::EnvPolicy,
    orchestration: ego_core::orchestrator::Mode,
    _model: Option<&str>,
) -> Result<Vec<RegisteredTool>, UsageError> {
    let mut all: Vec<_> = ego_tools::tools::builtins_with_environment(environment)
        .into_iter()
        .map(RegisteredTool::Effect)
        .collect();
    if selected.is_some_and(|names| names.iter().any(|name| name == "web_search")) {
        let local = all
            .iter()
            .find(|tool| tool.descriptor().name == "web_search")
            .and_then(RegisteredTool::as_effect)
            .cloned();
        all.retain(|tool| tool.descriptor().name != "web_search");
        all.push(RegisteredTool::Effect(
            ego_tools::tools::selected_web_search(local),
        ));
    }
    if profile == Some("coordinator") {
        all.push(RegisteredTool::Effect(Arc::new(ego_core::permit::Typed(
            ego_tools::tools::MemoryLookup,
        ))));
    }
    all.extend(
        ego_tools::orchestrator::tools_from_environment_with_mode(orchestration)
            .into_iter()
            .map(RegisteredTool::Effect),
    );
    all.push(RegisteredTool::HumanInput(Arc::new(AskQuestionTool)));
    all.push(RegisteredTool::Task);
    all.push(RegisteredTool::Goal);
    if let Some(source) = history {
        for tool in [
            ego_core::history::HistoryTool::Search,
            ego_core::history::HistoryTool::Open,
        ] {
            all.push(RegisteredTool::History {
                source: Arc::clone(&source),
                tool,
            });
        }
    }

    if let Some(skills) = skills.filter(|catalogue| !catalogue.specs().is_empty()) {
        all.push(RegisteredTool::skill(skills));
    }
    let Some(names) = selected else {
        return Ok(all);
    };
    for name in names {
        if !all
            .iter()
            .any(|tool| tool.descriptor().name == name.as_str())
        {
            if name == "web_search" {
                return Err(UsageError("--tools web_search: no search backend configured; set EXA_API_KEY to enable Exa (account credits and possible paid charges); no fallback is used".into()));
            }
            let known: Vec<String> = all.iter().map(|tool| tool.descriptor().name).collect();
            return Err(UsageError(format!(
                "--tools {name}: there is no such builtin; this build has {}",
                known.join(", ")
            )));
        }
    }
    Ok(all
        .into_iter()
        .filter(|tool| names.contains(&tool.descriptor().name))
        .collect())
}

/// The capability names `session.started` publishes.
///
/// Feature detection by name (`CLI.md` §4.2), so a consumer never parses a
/// version — and by *observation*, not by a hand-written list: a capability
/// this build does not have must not be advertised, and the cheapest way to
/// keep that true is to derive each one from the thing that provides it.
pub(super) fn capabilities(
    tools: &[RegisteredTool],
    instructions: bool,
    schema: bool,
) -> Vec<String> {
    let mut names = Vec::new();
    if !tools.is_empty() {
        names.push("tools".to_string());
    }
    if instructions {
        names.push("instructions".to_string());
    }
    if schema {
        names.push("schema".to_string());
    }
    names.push("jsonl".to_string());
    names
}

/// Everything the command line and the config layers settle before this run
/// touches a store, a provider, or a file of its own.
pub(super) struct Settings {
    /// `--timeout`, measured from settings resolution rather than the first
    /// turn so provider setup consumes the same absolute run budget.
    pub(super) run_deadline: Option<std::time::Instant>,
    /// The workspace root, and the directory relative paths resolve against.
    /// The same path today — [`workspace`] makes the root *be* the working
    /// directory — and two fields because they answer two questions.
    pub(super) root: PathBuf,
    pub(super) cwd: PathBuf,
    /// The effective runtime boundary, resolved independently of permission
    /// mode and fixed for the whole run.
    pub(super) confinement: RunConfinement,
    pub(super) trust: Trust,
    pub(super) profile_layers: Option<Vec<Sourced<ConfigLayer>>>,
    pub(super) effective: Effective,
}

/// Re-resolves rule transactions against the exact config authority captured at startup.
pub(crate) struct FrozenRuleResolver {
    pub(super) layers: Vec<Sourced<ConfigLayer>>,
    pub(super) trust: Trust,
    pub(super) selected_profile: Option<(String, Source)>,
    pub(super) selection: GenerationPolicySelection,
}

impl FrozenRuleResolver {
    pub(crate) fn new(
        layers: Vec<Sourced<ConfigLayer>>,
        trust: Trust,
        selected_profile: Option<(String, Source)>,
        selection: GenerationPolicySelection,
    ) -> Self {
        Self {
            layers,
            trust,
            selected_profile,
            selection,
        }
    }

    pub(super) fn resolve_rules(
        &self,
        snapshot: &RuleSnapshot,
    ) -> Result<Ruleset, PolicyControlError> {
        if snapshot.target != UserConfigTarget::Base
            || (!snapshot.permissions_present
                && (!snapshot.allow.is_empty() || !snapshot.deny.is_empty()))
        {
            return Err(PolicyControlError::Resolution {
                diagnostic: "a frozen policy resolution requires a valid base rule snapshot".into(),
            });
        }

        let mut layers = self.layers.clone();
        if let Some(user) = layers.iter_mut().find(|layer| layer.source == Source::User) {
            let captured_judge = user
                .value
                .permissions
                .as_ref()
                .and_then(|permissions| permissions.judge.clone());
            user.value.permissions = (snapshot.permissions_present || captured_judge.is_some())
                .then(|| PermCfg {
                    judge: captured_judge,
                    allow: snapshot.allow.clone(),
                    deny: snapshot.deny.clone(),
                });
        } else if snapshot.permissions_present {
            layers.push(Sourced {
                source: Source::User,
                value: ConfigLayer {
                    permissions: Some(PermCfg {
                        judge: None,
                        allow: snapshot.allow.clone(),
                        deny: snapshot.deny.clone(),
                    }),
                    ..ConfigLayer::default()
                },
            });
        }

        resolve_selected(
            &layers,
            self.trust,
            self.selected_profile
                .as_ref()
                .map(|(name, source)| (name.as_str(), *source)),
        )
        .map(|effective| effective.permissions)
        .map_err(|error| PolicyControlError::Resolution {
            diagnostic: error.to_string(),
        })
    }
}

impl FrozenPolicyResolver for FrozenRuleResolver {
    fn max_mode(&self) -> Result<Option<Mode>, PolicyControlError> {
        resolve_selected(
            &self.layers,
            self.trust,
            self.selected_profile
                .as_ref()
                .map(|(name, source)| (name.as_str(), *source)),
        )
        .map(|effective| effective.max_mode)
        .map_err(|error| PolicyControlError::Resolution {
            diagnostic: error.to_string(),
        })
    }

    fn resolve_rule_transaction(
        &self,
        before: &RuleSnapshot,
        after: &RuleSnapshot,
        affected: &[RuleMutation],
    ) -> Result<FrozenRuleResolution, PolicyControlError> {
        if before.target != UserConfigTarget::Base
            || after.target != UserConfigTarget::Base
            || before.target != after.target
        {
            return Err(PolicyControlError::Resolution {
                diagnostic: "a frozen policy resolution requires one base rule transaction".into(),
            });
        }

        let before_rules = self.resolve_rules(before)?;
        let after_rules = self.resolve_rules(after)?;
        let stored_changed = before != after;
        let comparisons = affected
            .iter()
            .map(|mutation| {
                RuleManagement::compare_resolved(
                    &before_rules,
                    &after_rules,
                    stored_changed,
                    &mutation.rule,
                    mutation.effect,
                )
            })
            .collect();
        let mode = self.selection.next_snapshot()?.mode;
        Ok(FrozenRuleResolution {
            next: GenerationPolicySnapshot {
                mode,
                ruleset: after_rules,
            },
            comparisons,
        })
    }
}

pub(super) fn handle_policy_request(
    request: PolicyControlRequest,
    mut apply: impl FnMut(PolicyControlRequest) -> Result<PolicyControlOutcome, PolicyControlError>,
    output: &dyn TextOutput,
    mut confirm_remember: impl FnMut(&RememberPreview) -> repl::Confirmation,
) -> Result<Option<repl::Line>, PolicyControlError> {
    let mismatch = || PolicyControlError::RuleManagement {
        diagnostic: "the policy-control outcome does not match its request".to_owned(),
    };
    let write_rows = |rows: Vec<String>| {
        for row in rows {
            output.line(&row);
        }
    };
    let warn = |receipt: &crate::rules::RuleMutationReceipt| {
        if let Some(warning) = crate::rules::publication_warning_text(&receipt.publication()) {
            output.note(&warning);
        }
    };

    match request {
        PolicyControlRequest::SetMode(mode) => match apply(PolicyControlRequest::SetMode(mode))? {
            PolicyControlOutcome::Mode { .. } => Ok(None),
            _ => Err(mismatch()),
        },
        PolicyControlRequest::Rule(mutation) => {
            let effect = mutation.effect;
            let PolicyControlOutcome::Rule { receipt, .. } =
                apply(PolicyControlRequest::Rule(mutation))?
            else {
                return Err(mismatch());
            };
            write_rows(crate::rules::render_mutation_receipt_rows(&receipt, effect));
            warn(&receipt);
            Ok(None)
        }
        PolicyControlRequest::Permission(PermissionInput::List) => {
            let PolicyControlOutcome::Permission(PermissionOutcome::List(rows)) =
                apply(PolicyControlRequest::Permission(PermissionInput::List))?
            else {
                return Err(mismatch());
            };
            write_rows(crate::rules::render_list_rows(&rows));
            Ok(None)
        }
        PolicyControlRequest::Permission(PermissionInput::Explain { call, workspace }) => {
            let PolicyControlOutcome::Permission(PermissionOutcome::Explain(explanation)) =
                apply(PolicyControlRequest::Permission(PermissionInput::Explain {
                    call: call.clone(),
                    workspace,
                }))?
            else {
                return Err(mismatch());
            };
            write_rows(crate::rules::render_explanation_rows(&call, &explanation));
            Ok(None)
        }
        PolicyControlRequest::Permission(PermissionInput::Revoke(id)) => {
            let PolicyControlOutcome::Rule { receipt, .. } = apply(
                PolicyControlRequest::Permission(PermissionInput::Revoke(id)),
            )?
            else {
                return Err(mismatch());
            };
            write_rows(crate::rules::render_revoke_receipt_rows(&receipt));
            warn(&receipt);
            Ok(None)
        }
        PolicyControlRequest::Permission(PermissionInput::Remember(request)) => {
            let PolicyControlOutcome::RememberPreview(preview) = apply(
                PolicyControlRequest::Permission(PermissionInput::Remember(request)),
            )?
            else {
                return Err(mismatch());
            };
            write_rows(crate::rules::render_remember_preview_rows(&preview));
            output.finish();
            match confirm_remember(&preview) {
                repl::Confirmation::Confirmed => {
                    let PolicyControlOutcome::Rule { receipt, .. } = apply(
                        PolicyControlRequest::Permission(PermissionInput::ConfirmRemember(preview)),
                    )?
                    else {
                        return Err(mismatch());
                    };
                    write_rows(crate::rules::render_remember_result_rows(
                        &UserConfigTarget::Base,
                        Some(&receipt),
                    ));
                    warn(&receipt);
                    Ok(None)
                }
                repl::Confirmation::Declined => {
                    write_rows(crate::rules::render_remember_result_rows(
                        &UserConfigTarget::Base,
                        None,
                    ));
                    Ok(None)
                }
                repl::Confirmation::Propagate(line) => Ok(Some(line)),
            }
        }
        PolicyControlRequest::Permission(PermissionInput::ConfirmRemember(_)) => Err(mismatch()),
    }
}

/// Validated direct-host facts that exist only for the explicit CI posture.
pub(super) struct CiInvocation {
    pub(super) session_root: PathBuf,
    pub(super) artifact_root: PathBuf,
}

pub(super) fn ci_invocation(
    args: &Args,
    settings: &Settings,
    driver: &Driver<'_>,
) -> Result<Option<CiInvocation>, UsageError> {
    if settings
        .effective
        .active_profile
        .as_ref()
        .is_none_or(|profile| profile.name != "ci")
    {
        return Ok(None);
    }
    if !matches!(driver, Driver::Once) {
        return Err(UsageError(
            "profile `ci` is available only through ego run or ego resume".to_owned(),
        ));
    }
    if args.instructions.is_none() {
        return Err(UsageError(
            "profile `ci` requires --instructions with an explicit file".to_owned(),
        ));
    }
    if args.prompt.is_none()
        && args.agent_file.is_none()
        && args.stdin == crate::cli::Stdin::Ambient
        && args.answers.is_empty()
    {
        return Err(UsageError(
            "profile `ci` requires a headless prompt".to_owned(),
        ));
    }

    let session_root = args
        .session_dir
        .as_ref()
        .or_else(|| {
            settings
                .effective
                .session_dir
                .as_ref()
                .map(|root| &root.value)
        })
        .ok_or_else(|| {
            UsageError(
                "profile `ci`: --session-dir must be an absolute, pre-existing directory"
                    .to_owned(),
            )
        })?;
    let artifact_root = args
        .artifact_dir
        .as_ref()
        .or_else(|| {
            settings
                .effective
                .artifact_dir
                .as_ref()
                .map(|root| &root.value)
        })
        .ok_or_else(|| {
            UsageError(
                "profile `ci`: --artifact-dir must be an absolute, pre-existing directory"
                    .to_owned(),
            )
        })?;
    let session_root = isolated_ci_root("--session-dir", session_root)?;
    let artifact_root = isolated_ci_root("--artifact-dir", artifact_root)?;
    if roots_overlap(&settings.root, &session_root)
        || roots_overlap(&settings.root, &artifact_root)
        || roots_overlap(&session_root, &artifact_root)
    {
        return Err(UsageError(
            "workspace, session, and artifact roots must be pairwise disjoint".to_owned(),
        ));
    }
    Ok(Some(CiInvocation {
        session_root,
        artifact_root,
    }))
}

pub(super) fn isolated_ci_root(flag: &str, path: &Path) -> Result<PathBuf, UsageError> {
    if !path.is_absolute() {
        return Err(UsageError(format!(
            "{flag} must be an absolute, pre-existing directory"
        )));
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        UsageError(format!(
            "{flag} must be an absolute, pre-existing directory"
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(UsageError(format!(
            "{flag} must be an absolute, pre-existing directory whose final component is not a symlink"
        )));
    }
    std::fs::canonicalize(path).map_err(|error| {
        UsageError(format!(
            "{flag} directory {} could not be resolved: {error}",
            path.display()
        ))
    })
}

pub(super) fn roots_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

pub(super) fn ci_preflight_termination(ci: bool, termination: RunTermination) -> RunTermination {
    if !ci {
        return termination;
    }
    match termination {
        RunTermination::UsageError { message }
            if message.to_ascii_lowercase().contains("credential") =>
        {
            RunTermination::UsageError {
                message: format!("profile `ci` requires a non-interactive credential: {message}"),
            }
        }
        other => other,
    }
}

/// The command line and the config layers, resolved into one answer.
pub(super) fn settings(
    args: &Args,
    trust_for: impl FnOnce(&Path) -> Trust,
    diagnostics: &dyn Diagnostics,
) -> Result<Settings, UsageError> {
    let cwd = workspace(args.directory.as_ref())?;
    rooted_settings(args, cwd, trust_for, diagnostics)
}

/// Apply the Windows direct-CLI admission after configuration resolution and
/// before any provider, session, or user-code effect.
pub(super) fn admit_windows_unconfined_start(
    args: &Args,
    settings: &mut Settings,
    driver: &Driver<'_>,
    home: Option<&Path>,
    diagnostics: &dyn Diagnostics,
) -> Result<(), UsageError> {
    let Some(confinement) = windows_unconfined::admit(
        args,
        settings.effective.sandbox,
        settings.confinement.offline(),
        driver,
        &settings.root,
        home,
        diagnostics,
    )?
    else {
        return Ok(());
    };

    settings.effective.sandbox = Some(confinement);
    settings.effective.sandbox_provenance =
        ego_core::config::SettingProvenance::UnconfinedAcknowledgement;
    settings.confinement = ego_tools::exec::admit_confinement(
        confinement,
        settings.confinement.offline(),
        CanonPath::new(&settings.root),
    )
    .map_err(|error| UsageError(error.to_string()))?
    .with_roots(settings.confinement.roots().to_vec())
    .with_exec_paths(settings.confinement.exec_paths().to_vec());
    Ok(())
}

pub(super) fn rooted_settings(
    args: &Args,
    cwd: PathBuf,
    trust_for: impl FnOnce(&Path) -> Trust,
    diagnostics: &dyn Diagnostics,
) -> Result<Settings, UsageError> {
    // The workspace root is the working directory. Discovering a repository
    // root is `git`'s question and would make the answer depend on a tool that
    // may not be installed; `-C` is how a caller says otherwise.
    let root = cwd.clone();

    // Trust comes from an operator answer or a valid standing grant. A
    // headless caller never asks, so no
    // terminal or stdin read is reachable from that path.
    let trust = trust_for(&root);
    let layers = config::layers(&root, args, trust)?;
    let selected_profile = config::profile(args);
    let effective = config::resolve_session_profile(
        &layers,
        trust,
        selected_profile
            .as_ref()
            .map(config::SelectedProfile::as_resolver_input),
        args.ceiling_profile.as_deref(),
        &cwd,
    )
    .map_err(|err| UsageError(err.to_string()))?;
    // Checked here, where the posture and the trust it was resolved from are
    // the same pair. A frozen run resolves its posture from the trust its
    // capture pinned and then reports `Untrusted` — it is a child, not a seat
    // — so the same check there compares two different answers and fails on a
    // correct run.
    if trust == Trust::Untrusted || args.bare {
        debug_assert_eq!(effective.posture, Posture::Bare);
    }
    finish_rooted_settings(args, cwd, effective, trust, Some(layers), diagnostics)
}

pub(super) fn frozen_rooted_settings(
    args: &Args,
    cwd: PathBuf,
    frozen: &config::FrozenRunConfig,
    diagnostics: &dyn Diagnostics,
) -> Result<Settings, UsageError> {
    // The capture's own posture, not `Untrusted`. A child runs at the posture
    // its parent's answer settled: reporting anything else here is a second
    // answer to a question already answered, and the consumers that read the
    // posture rather than the resolved layers — the hook catalogue is the one
    // that matters — acted on it.
    let mut rooted_args = args.clone();
    rooted_args.directory = Some(cwd.clone());
    let layers = frozen.layers(&rooted_args)?;
    let effective = frozen.resolve_layers(&layers, &rooted_args)?;
    finish_rooted_settings(
        args,
        cwd,
        effective,
        frozen.trust(),
        Some(layers),
        diagnostics,
    )
}

pub(super) fn finish_rooted_settings(
    args: &Args,
    cwd: PathBuf,
    mut effective: Effective,
    trust: Trust,
    profile_layers: Option<Vec<Sourced<ConfigLayer>>>,
    diagnostics: &dyn Diagnostics,
) -> Result<Settings, UsageError> {
    config::report_discarded(&cwd, &effective, diagnostics);
    let started = std::time::Instant::now();
    let timeout = match args.timeout.as_deref() {
        Some(spec) => Some(deadline(spec)?),
        None => None,
    };
    let run_deadline = timeout
        .map(|timeout| {
            started
                .checked_add(timeout)
                .ok_or_else(|| UsageError("--timeout exceeds the monotonic clock".to_string()))
        })
        .transpose()?;
    let root = cwd.clone();

    // §4 requires the clamp be stated, not silent: a `--budget 20$` reduced to
    // an administrator's $5 must not read as binding at $20. The money half
    // travels into the gate, which publishes it on every `MeteringSelected`;
    // the token half has no such carrier, so it is said here.
    if let Some(asked) = effective.clamped.tokens {
        diagnostics.note(&format!(
            "the token ceiling of {asked} was reduced to {} by a system ceiling",
            effective
                .ceilings
                .tokens
                .expect("a clamp leaves a ceiling in force")
        ));
    }

    if let Some(home) = std::env::var_os("HOME") {
        for warning in super::profile_roots::defaults(&mut effective, Path::new(&home)) {
            diagnostics.note(&warning);
        }
    }
    if let Some(directory) = &args.read_only_dir {
        super::read_directory::apply(&mut effective, &root, directory)?;
        diagnostics.note(&format!(
            "read-only launch directory: {}",
            directory.display()
        ));
    }
    let roots =
        super::profile_roots::admit(&effective, &CanonPath::new(&root)).map_err(UsageError)?;

    let exec_paths = effective
        .exec_paths
        .iter()
        .map(|path| {
            std::fs::canonicalize(path)
                .map(CanonPath::new)
                .map_err(|error| {
                    UsageError(format!(
                        "cannot resolve exec root {}: {error}",
                        path.display()
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let confinement = ego_tools::exec::admit_confinement(
        effective.sandbox.unwrap_or_default(),
        args.offline || effective.network == ego_core::config::Network::Off,
        CanonPath::new(&root),
    )
    .map_err(|err| UsageError(err.to_string()))?
    .with_exec_paths(exec_paths)
    .with_roots(roots);

    Ok(Settings {
        run_deadline,
        root,
        cwd,
        confinement,
        trust,
        profile_layers,
        effective,
    })
}

/// What a turn is made of: the prompt, its system half, the tools on offer,
/// the schema the answer must satisfy, and the name of the model to ask.
pub(super) struct Materials {
    pub(super) system: Option<SystemPrompt>,
    pub(super) registry: Vec<RegisteredTool>,
    pub(super) skills: Option<Arc<crate::skill::SkillCatalogue>>,
    pub(super) compiled: Option<schema::Schema>,
    pub(super) text: String,
    pub(super) slug: String,
}

/// Everything a turn needs that comes from a file, a flag or stdin.
///
/// Ordinary runs gather these before choosing a store or resolving a model.
/// Resume answers gather them only after confidential policy recovery, so
/// their filesystem, diagnostics and skill-discovery effects cannot precede
/// that recovery gate.
pub(super) fn materials(
    settings: &Settings,
    args: &Args,
    seeded: bool,
    history: Arc<dyn ego_core::history::SessionHistory>,
    diagnostics: &dyn Diagnostics,
) -> Result<Materials, UsageError> {
    let text = if let Some(agent_file) = &args.agent_file {
        if seeded {
            prompt::seed_agent_file(agent_file.body(), args.stdin, &prompt::ProcessStdin)?
        } else {
            prompt::assemble_agent_file(agent_file.body(), args.stdin, &prompt::ProcessStdin)?
        }
    } else if seeded {
        prompt::seed(args.prompt.as_deref(), args.stdin, &prompt::ProcessStdin)?
    } else {
        prompt::assemble(args.prompt.as_deref(), args.stdin, &prompt::ProcessStdin)?
    };
    prepared_materials(settings, args, text, history, diagnostics)
}

pub(super) fn prepared_materials(
    settings: &Settings,
    args: &Args,
    text: String,
    history: Arc<dyn ego_core::history::SessionHistory>,
    diagnostics: &dyn Diagnostics,
) -> Result<Materials, UsageError> {
    let schema_spec = args.schema.as_deref().map(|spec| (spec, None)).or_else(|| {
        args.agent_file
            .as_ref()
            .and_then(|file| file.schema().map(|spec| (spec, Some(file.path()))))
    });
    let compiled = match schema_spec {
        Some((spec, origin)) => {
            let source = rooted_at_file(spec, &settings.cwd).map_err(|error| match origin {
                Some(agent_file) => UsageError(format!(
                    "agent file {} selected a schema that could not be read: {error}",
                    agent_file.display()
                )),
                None => error,
            })?;
            Some(schema::Schema::compile(&source)?)
        }
        None => None,
    };
    let system = instructions(
        &settings.effective,
        args,
        &settings.root,
        &settings.cwd,
        diagnostics,
    )?;
    let skills = discover_skills(settings, diagnostics)?;
    let skill_capture = skills
        .as_ref()
        .map(|catalogue| catalogue.capture())
        .transpose()
        .map_err(|error| UsageError(error.to_string()))?;
    let registry = tools_with_mode(
        args.tools.as_ref(),
        skill_capture
            .as_ref()
            .map(crate::skill::SkillCatalogueCapture::core),
        settings
            .effective
            .active_profile
            .as_ref()
            .map(|profile| profile.name.as_str()),
        Some(history),
        settings
            .effective
            .exec
            .policy()
            .map_err(|error| UsageError(error.to_string()))?,
        orchestration_mode(&settings.effective),
        args.model.as_deref().or_else(|| {
            settings
                .effective
                .model
                .as_ref()
                .map(|model| model.0.as_str())
        }),
    )?;
    let slug = args
        .model
        .clone()
        .or_else(|| settings.effective.model.clone().map(|model| model.0))
        .ok_or_else(|| {
            UsageError(
                "no model: pass --model provider/model, set EGO_MODEL, or put \
                 `model = \"…\"` in a config file"
                    .to_string(),
            )
        })?;

    Ok(Materials {
        system,
        registry,
        skills,
        compiled,
        text,
        slug,
    })
}

pub(super) fn discover_skills(
    settings: &Settings,
    diagnostics: &dyn Diagnostics,
) -> Result<Option<Arc<crate::skill::SkillCatalogue>>, UsageError> {
    discover_skills_at(
        &settings.root,
        settings.trust,
        settings.profile_layers.as_deref(),
        diagnostics,
    )
}

pub(super) fn discover_skills_at(
    root: &Path,
    trust: Trust,
    profile_layers: Option<&[Sourced<ConfigLayer>]>,
    diagnostics: &dyn Diagnostics,
) -> Result<Option<Arc<crate::skill::SkillCatalogue>>, UsageError> {
    let Some(profile_layers) = profile_layers else {
        return Ok(None);
    };
    let Some(discovery) =
        crate::skill::SkillDiscovery::for_workspace(root, trust == Trust::Trusted)
    else {
        return Ok(None);
    };
    let actions = repl::actions::ActionRegistry::final_surface();
    let fixed = actions
        .names()
        .into_iter()
        .chain(actions.aliases())
        .collect::<Vec<_>>();
    let catalogue = crate::skill::SkillCatalogue::discover_with_profile_layers(
        discovery,
        &fixed,
        profile_layers,
    )
    .map_err(|error| UsageError(error.to_string()))?;
    let capture = catalogue
        .capture()
        .map_err(|error| UsageError(error.to_string()))?;
    for diagnostic in capture.diagnostics() {
        diagnostics.note(&diagnostic.to_string());
    }
    Ok(Some(Arc::new(catalogue)))
}

/// The store this run logs through, and which session in it.
pub(super) struct Chosen {
    pub(super) store: Arc<JsonlStore>,
    pub(super) id: SessionId,
}

/// *Which* session this run continues or creates — not the opening of it.
///
/// The two halves are separated on purpose, and this is the half that happens
/// before the model resolves. Choosing is a read-only catalogue query and it
/// can refuse: `ego resume` in a directory that owns no session is a usage
/// error about the command as typed, and it would be true whatever the
/// provider said. Answering it *after* the model resolved told an operator
/// standing in the wrong directory that the network was down. Opening stays in
/// [`execute`], because foreign continuation recovery needs the replayed state.
/// The invocation-confined MCP registry reconciles before the writer admits
/// the run context.
pub(super) fn chosen(
    args: &Args,
    root: &Path,
    store_override: Option<&Path>,
) -> Result<Chosen, UsageError> {
    let store_root = match store_override {
        Some(path) => path.to_path_buf(),
        None => session::store_root(args.session_dir.as_ref())?,
    };
    let store_root = rooted_path(root, store_root);
    // The same `root` that derives a named session's id below, so "the session
    // called build in this workspace" and "the sessions belonging to this
    // workspace" cannot disagree about which workspace that is.
    let store = Arc::new(session::jsonl(store_root, root.to_path_buf()));
    let id = session::id_for(&args.session, root, store.as_ref())?;

    Ok(Chosen { store, id })
}

/// Build the fixed judge generation envelope used by both startup proofs.
///
/// Dynamic authority facts occupy one user-text block and cannot change the
/// request's tool, thinking, parallelism or output-ceiling requirements. A
/// canonical write-shaped input therefore exercises the same provider-facing
/// shape without sending a generation or pretending that the provider was
/// contacted. The exact in-turn request is checked again by `TurnLoop`.
pub(crate) fn fixed_judge_request(
    identity: &ModelRef,
    request_model: &str,
    max_output_tokens: Option<u64>,
    confinement: &RunConfinement,
    exec_caps: ExecCaps,
) -> GenRequest {
    let path = confinement.workspace().clone();
    let call = PreparedCall::new(
        "write",
        Op::Write {
            path: path.clone(),
            change: Change::Whole(Vec::new()),
        },
    );
    let escalation = Escalation::ModeAsks {
        mode: Mode::Auto,
        op: OpName::Write,
        path,
    };
    JudgeInput::new(&call, confinement, exec_caps, &escalation).request(&JudgeConfig {
        identity: identity.clone(),
        request_model: request_model.to_owned(),
        max_output_tokens,
    })
}

pub(crate) fn validate_judge_request(
    identity: &ModelRef,
    capabilities: &ModelCapabilities,
    request: &GenRequest,
    ensure_expressible: impl FnOnce(&GenRequest) -> Result<(), RequestExpressibilityError>,
) -> Result<(), String> {
    let gaps = capabilities.request_gaps(request);
    if !gaps.is_empty() {
        return Err(format!(
            "judge model `{}` is incompatible with the fixed judge request: {}",
            identity.0.as_str(),
            gaps.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    ensure_expressible(request).map_err(|error| {
        format!(
            "judge model `{}` cannot encode the fixed judge request: {error}",
            identity.0.as_str()
        )
    })
}

pub(super) fn preflight_judge(
    judge: &GenerationProfile,
    confinement: &RunConfinement,
    exec_caps: ExecCaps,
) -> Result<(), RunTermination> {
    let mut request = fixed_judge_request(
        &judge.identity,
        &judge.request_model,
        judge.output_ceiling(),
        confinement,
        exec_caps,
    );
    request.thinking = judge.thinking;
    validate_judge_request(&judge.identity, &judge.capabilities, &request, |request| {
        judge.generator.ensure_expressible(request)
    })
    .map_err(|message| RunTermination::UsageError { message })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CapturedAutoPreflight {
    DeferredDynamic,
    Valid,
    Refused { diagnostic: String },
}

pub(super) struct CliAutoPreflight {
    pub(super) state: CapturedAutoPreflight,
}

impl CliAutoPreflight {
    pub(super) fn new(state: CapturedAutoPreflight) -> Self {
        Self { state }
    }

    /// Capture only supplied request-shape facts. Dynamic facts defer to generation preflight.
    pub(super) fn capture(effective: &Effective, confinement: &RunConfinement) -> Self {
        if effective
            .judge
            .as_ref()
            .is_some_and(|model| model.0 == "none")
        {
            return Self::new(CapturedAutoPreflight::Valid);
        }
        let Some(identity) = effective.judge.as_ref().or(effective.model.as_ref()) else {
            return Self::new(CapturedAutoPreflight::DeferredDynamic);
        };
        let profile = match model::supplied_request_profile(identity) {
            Ok(Some(profile)) => profile,
            Ok(None) => return Self::new(CapturedAutoPreflight::DeferredDynamic),
            Err(error) => {
                return Self::new(CapturedAutoPreflight::Refused {
                    diagnostic: error.message,
                });
            }
        };
        let mut request = fixed_judge_request(
            identity,
            profile.request_model(),
            profile.output_ceiling(),
            confinement,
            ExecCaps::empty(),
        );
        request.thinking = match model::judge_thinking(profile.capabilities()) {
            Ok(thinking) => thinking,
            Err(error) => {
                return Self::new(CapturedAutoPreflight::Refused {
                    diagnostic: format!(
                        "judge model `{}` is incompatible with the fixed judge request: {error}",
                        identity.0,
                    ),
                });
            }
        };
        let state =
            match validate_judge_request(identity, profile.capabilities(), &request, |request| {
                profile.ensure_expressible(request)
            }) {
                Ok(()) => CapturedAutoPreflight::Valid,
                Err(diagnostic) => CapturedAutoPreflight::Refused { diagnostic },
            };
        Self::new(state)
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        match &self.state {
            CapturedAutoPreflight::Refused { diagnostic } => Err(diagnostic.clone()),
            CapturedAutoPreflight::DeferredDynamic | CapturedAutoPreflight::Valid => Ok(()),
        }
    }

    pub(super) fn validate_startup(&self) -> Result<(), RunTermination> {
        self.validate()
            .map_err(|message| RunTermination::UsageError { message })
    }
}

impl PreflightPort for CliAutoPreflight {
    fn validate_auto(&self) -> Result<(), PolicyControlError> {
        self.validate()
            .map_err(|diagnostic| PolicyControlError::JudgeUnavailable { diagnostic })
    }
}

pub(super) fn generation_refusal(error: GenerationModelError) -> RunTermination {
    match error.class {
        GenerationModelErrorClass::Usage => RunTermination::UsageError {
            message: error.message,
        },
        GenerationModelErrorClass::Provider => RunTermination::Provider {
            message: error.message,
        },
        GenerationModelErrorClass::Internal => RunTermination::Failed {
            message: error.message,
        },
    }
}

pub(super) struct GenerationPreflight {
    pub(super) source: Arc<model::CliGenerationModelSource>,
    pub(super) selection: Arc<model::GenerationSelection>,
    pub(super) capabilities: ModelCapabilities,
    pub(super) judge_available: bool,
    pub(super) proof_directory: tempfile::TempDir,
    pub(super) proof_evidence: SessionEvidenceStore,
}

pub(super) struct GenerationPreflightInput<'a> {
    pub(super) session_id: SessionId,
    pub(super) confinement: &'a RunConfinement,
    pub(super) effective: &'a Effective,
    pub(super) slug: String,
    pub(super) cancel: &'a CancelToken,
    pub(super) run_deadline: Option<std::time::Instant>,
    pub(super) mcp_registry: Option<Arc<McpRegistryController>>,
    pub(super) native_tools: &'a [RegisteredTool],
}

pub(super) async fn generation_preflight(
    input: GenerationPreflightInput<'_>,
    status: &mut Option<InteractiveStatus>,
) -> Result<GenerationPreflight, RunTermination> {
    let run_deadline = input.run_deadline;
    let cancel = input.cancel;
    let mut running = Box::pin(generation_preflight_inner(input, status));
    tokio::select! {
        biased;
        cause = stop_signal(run_deadline) => {
            cancel.cancel(cause);
            let _ = running.as_mut().await;
            Err(cancelled_termination(cancel))
        }
        result = &mut running => {
            if run_deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                cancel.cancel(CancelCause::Timeout);
            }
            if cancel.is_cancelled() {
                Err(cancelled_termination(cancel))
            } else {
                result
            }
        }
    }
}

pub(super) async fn generation_preflight_inner(
    input: GenerationPreflightInput<'_>,
    status: &mut Option<InteractiveStatus>,
) -> Result<GenerationPreflight, RunTermination> {
    let GenerationPreflightInput {
        session_id,
        confinement,
        effective,
        slug,
        cancel,
        run_deadline,
        mcp_registry,
        native_tools,
    } = input;
    let proof_directory = tempfile::tempdir().map_err(|error| RunTermination::Failed {
        message: format!("could not create the sandbox proof store: {error}"),
    })?;
    let proof_evidence =
        SessionEvidenceStore::open(proof_directory.path(), session_id, EVIDENCE_CAP).map_err(
            |error| RunTermination::Failed {
                message: format!("could not open the sandbox proof store: {error}"),
            },
        )?;
    let (proof_ports, proof_observation, _) =
        effect_scope(confinement, &proof_evidence, cancel, run_deadline, None).await?;
    let http = Arc::new(ProviderHttpAdapter::new(proof_ports)) as Arc<dyn ego_ai::http::Http>;

    let mode = effective.mode.unwrap_or(Mode::Default);
    let judge_identity = effective.judge.clone();
    let mut thinking = effective.thinking;
    let human_picker = if status.as_ref().is_some_and(|status| status.human) {
        let catalogue_snapshot = Arc::new(
            models::build_catalogue_snapshot(
                &ModelsArgs {
                    provider: None,
                    refresh: false,
                    json: false,
                },
                &http,
            )
            .await?,
        );
        let home = config::ego_home().ok_or_else(|| RunTermination::Failed {
            message: "the model picker needs EGO_HOME or HOME for user configuration".to_string(),
        })?;
        let config_store: Arc<dyn UserConfigStore> = Arc::new(OsUserConfigStore::new(
            Arc::new(OsIo),
            home.join("config.toml"),
        ));
        if thinking.is_none()
            && crate::model_picker::available_entry(&catalogue_snapshot, &slug)
                .is_ok_and(|row| row.capabilities.default_reasoning_effort.is_none())
        {
            let picker =
                ModelPicker::for_startup_reasoning(Arc::clone(&catalogue_snapshot), slug.clone())
                    .map_err(|error| RunTermination::UsageError {
                    message: error.to_string(),
                })?;
            match status
                .as_ref()
                .expect("a human interactive status was checked above")
                .seat
                .read_startup_model(Arc::new(Mutex::new(picker)), None)
            {
                crate::model_picker::PickerOutcome::ModelSwitch(request) => {
                    let publication = crate::onboarding::persist_startup_selection(
                        config_store.as_ref(),
                        &request,
                    )
                    .map_err(|error| RunTermination::UsageError {
                        message: format!("could not persist the startup reasoning choice: {error}"),
                    })?;
                    if let UserConfigPublication::PublishedDurabilityUncertain { diagnostic } =
                        publication
                    {
                        status
                            .as_ref()
                            .expect("a human interactive status was checked above")
                            .seat
                            .set_notice(format!(
                                "model defaults are authoritative with a durability warning: {diagnostic}"
                            ));
                    }
                    thinking = Some(request.thinking());
                }
                crate::model_picker::PickerOutcome::Cancelled => {
                    cancel.cancel(CancelCause::User);
                    return Err(cancelled_termination(cancel));
                }
            }
        }
        Some((catalogue_snapshot, config_store))
    } else {
        None
    };
    let source = Arc::new(
        model::CliGenerationModelSource::new(
            ego_core::config::ModelRef(slug),
            thinking,
            judge_identity,
            effective.ceilings.clone(),
            effective.auth.clone(),
        )
        .with_output_limit(effective.max_output_tokens)
        .with_offline(confinement.offline()),
    );
    let selection = source.selection();
    let bound = source.bind(Arc::clone(&http), cancel.clone(), run_deadline);
    let main = bound
        .resolve(GenerationModelRequest::new(GenerationRole::Main))
        .await
        .map_err(generation_refusal)?;
    if let Some(notice) =
        model::reasoning_default_notice(&main.identity, &main.capabilities, thinking)
    {
        if let Some(status) = status.as_ref() {
            status.seat.set_notice(notice.to_owned());
        } else {
            crate::output::note(notice);
        }
    }
    if let Some((catalogue_snapshot, config_store)) = human_picker {
        let picker = Arc::new(Mutex::new(ModelPicker::for_selection(
            Arc::clone(&catalogue_snapshot),
            Arc::clone(&selection),
            Arc::clone(&config_store),
        )));
        let status = status
            .as_mut()
            .expect("a human interactive status was checked above");
        status
            .seat
            .install_model_picker(
                Arc::clone(&picker),
                Arc::clone(&selection),
                Arc::clone(&config_store),
                mcp_registry
                    .as_ref()
                    .map(Arc::clone)
                    .ok_or_else(|| RunTermination::Failed {
                        message: "interactive MCP registry authority is unavailable".to_owned(),
                    })?,
                native_tools.to_vec(),
            )
            .map_err(|error| RunTermination::Failed {
                message: format!("interactive actions could not be installed: {error}"),
            })?;
        status.picker = Some(picker);
        status.model_config_store = Some(config_store);
        debug_assert!(
            status.picker.is_some(),
            "the run retains the installed picker"
        );
    }
    let capabilities = main.capabilities.clone();
    let judge_available = !effective
        .judge
        .as_ref()
        .is_some_and(|model| model.0 == "none");
    if mode == Mode::Auto && judge_available {
        let judge = bound
            .resolve(GenerationModelRequest::new(GenerationRole::Judge))
            .await
            .map_err(generation_refusal)?;
        preflight_judge(&judge, confinement, proof_observation.capabilities())?;
    }

    Ok(GenerationPreflight {
        source,
        selection,
        capabilities,
        judge_available,
        proof_directory,
        proof_evidence,
    })
}
