//! What drives the turns once the session is open.
//!
//! `execute` sets a session up; this is what happens to it afterwards, in
//! either posture: one prompt from argv or stdin, or lines from a person until
//! they stop. The setup is identical either way, so it is the tail that
//! branches rather than the whole function that is duplicated.
//!
//! Glob-imported from the parent for the same reason as `executor`: this is
//! `execute`'s own tail, split out for navigability, and it reaches back into
//! the session the caller has already opened.
use super::*;

/// What drives the turns once the session is open.
///
/// The setup above is identical either way — the same resolution, the same
/// session, the same policy — so it is the tail that branches rather than the
/// function that is duplicated.
pub(super) enum Driver<'a> {
    /// One prompt from argv or stdin, and the run is over.
    Once,
    /// Lines from a person, until they stop.
    Interactive {
        seat: Arc<dyn InteractiveSeat>,
        /// Run after each turn: the screen holds the last block back until it
        /// knows no more is coming, and between turns is when it knows.
        between: &'a (dyn Fn() + Send + Sync),
        /// Continue only the same session's root-admission boundary.
        resume_after_root: Option<ego_core::event::id::TurnId>,
        root_transition: RootTransitionSlot,
        /// The opening argv, stdin or agent-file prompt is consumed only once.
        seed_opening_prompt: bool,
    },
}

pub(super) struct RunHost<'a> {
    pub(super) driver: Driver<'a>,
    pub(super) elicitor: Option<Arc<dyn Elicitor>>,
    pub(super) status: Option<InteractiveStatus>,
    pub(super) diagnostics: &'a dyn Diagnostics,
    pub(super) text_output: Option<&'a dyn TextOutput>,
    pub(super) evidence: Arc<EvidenceAdmissionService>,
}

pub(super) struct InteractiveStatus {
    pub(super) seat: Arc<repl::Seat>,
    pub(super) width: usize,
    pub(super) human: bool,
    pub(super) native: bool,
    /// The run owns the same picker instance installed into the terminal seat.
    pub(super) picker: Option<Arc<Mutex<ModelPicker>>>,
    pub(super) model_config_store: Option<Arc<dyn UserConfigStore>>,
}

/// `home` is where a remembered answer lives; `None` is a process with no ego
/// home at all, which asks every launch and records nothing.
///
/// `--bare` is already an explicit refusal, so it reaches neither the store
/// nor the seat — it does not read a remembered answer and does not write one.
pub(super) fn run_trust(
    args: &Args,
    driver: &Driver<'_>,
    root: &Path,
    home: Option<&Path>,
    diagnostics: &dyn Diagnostics,
) -> Trust {
    let selected = args
        .instructions
        .as_ref()
        .and_then(|path| explicit(path, root).ok());
    if args.bare {
        return Trust::Untrusted;
    }
    crate::trust::resolve_selected(
        root,
        home,
        diagnostics,
        args.trust_workspace,
        args.trust_development_programs,
        selected.as_deref(),
        |context| match driver {
            Driver::Interactive { seat, .. } if seat.has_human() => Some(seat.trust(root, context)),
            _ => None,
        },
    )
}

/// The seat, as the run loop needs it.
///
/// A trait so a test can drive the whole interactive path from a script of
/// lines, with no terminal and no editor — the real [`repl::Seat`] needs a tty
/// and would make every one of those tests unrunnable in CI.
pub trait Reads {
    /// Read one line, blocking until there is one.
    fn read(&self) -> repl::Line;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteboardKind {
    Image,
    Text,
    Empty,
}

/// The two explicit decisions only a human-driven run may make.
pub trait InteractiveSeat: Reads + std::fmt::Debug + Send + Sync {
    fn has_human(&self) -> bool;
    fn width(&self) -> usize {
        80
    }
    fn action_help(&self) -> String {
        crate::repl::render_action_help(
            &crate::repl::actions::ActionRegistry::final_surface(),
            self.width().saturating_sub(4),
        )
    }
    fn action_help_details(&self) -> String {
        crate::repl::render_action_help_details(
            &crate::repl::actions::ActionRegistry::final_surface(),
            self.width().saturating_sub(4),
        )
    }
    fn key_hints(&self) -> String {
        crate::repl::render_key_hints(
            &crate::repl::actions::ActionRegistry::final_surface(),
            self.width(),
        )
    }
    fn startup_key_hints(&self) -> String {
        crate::repl::render_startup_key_hints(
            &crate::repl::actions::ActionRegistry::final_surface(),
            self.width(),
        )
    }
    /// Collect a persistent model choice before the first run is materialized.
    fn read_startup_model(
        &self,
        picker: Arc<Mutex<ModelPicker>>,
        notice: Option<String>,
    ) -> crate::model_picker::PickerOutcome;
    /// Host inspections and image admission share the idle/busy driver inbox.
    fn next_inspection(&self) -> futures::future::BoxFuture<'_, repl::HostCommand> {
        Box::pin(futures::future::pending())
    }
    /// Read one line, or `None` when the seat stays idle until `deadline`.
    /// Seats without an idle period never time out.
    fn read_before(&self, _deadline: std::time::Instant) -> Option<repl::Line> {
        Some(self.read())
    }
    /// Wake an idle read without replacing the composer draft.
    fn wake_background_exec(&self) {}
    fn busy(&self) {}
    /// Mark native compaction work without presenting it as a model reply.
    fn compacting(&self) {
        self.busy();
    }
    fn idle(&self) {}
    /// Report `message` as the host error state until the next turn starts.
    fn hold_error(&self, _message: &str) {}
    /// How long the current or last turn waited on a modal.
    fn modal_wait(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
    fn set_image_chips(&self, _chips: Vec<String>) {}
    /// The current text draft owns this handle, including across stash/restore.
    fn image_draft(&self) -> Option<repl::ImageDraft> {
        None
    }
    fn set_notice(&self, _notice: String) {}
    fn pasteboard_kind(&self) -> Result<PasteboardKind, String> {
        Err("OS pasteboard is unavailable".to_owned())
    }
    fn pasteboard_image(&self, _max_bytes: u64) -> Result<Vec<u8>, String> {
        Err("OS pasteboard image is unavailable".to_owned())
    }
    fn pasteboard_text(&self) -> Result<String, String> {
        Err("OS pasteboard text is unavailable".to_owned())
    }
    fn insert_paste_text(&self, _text: String) {}
    /// Hand the session-long core control endpoint to the turn coordinator.
    fn take_control(&self) -> InteractiveControl {
        let (_ingress, control) = interactive_control();
        control
    }
    /// Return terminal ingress ownership before re-entering rooted admission.
    fn restore_control(&self, _control: InteractiveControl) {}
    /// Publish live queue facts from the authoritative reduced session.
    fn refresh_queue(&self, _state: &SessionState) {}
    /// Publish the coordinator-owned compaction guard to the interactive surface.
    fn set_compaction_availability(&self, _availability: repl::actions::Availability) {}
    /// Confirm a source-owned clean-session transition.
    fn confirm_compaction(&self, _source: SessionId) -> bool {
        false
    }
    /// Select an existing session through the terminal owner.
    fn pick_session(
        &self,
        _source: crate::catalogue::ResumePicker,
    ) -> crate::catalogue::SessionPick {
        crate::catalogue::SessionPick::Cancelled
    }
    /// Switch the terminal and its draft namespace after publication.
    fn activate_session(&self, _target: SessionId) -> bool {
        false
    }
    fn trust(&self, workspace: &Path, context: &str) -> Trust;
    /// Acknowledge, once for this user, the Windows host's lack of confinement.
    /// Test and non-human seats deny unless they explicitly own this authority.
    fn acknowledge_windows_unconfined(&self, _workspace: &Path) -> bool {
        false
    }
    fn permission(&self, request: &PermissionRequest) -> PermissionDecision;
    /// Await a modal decision without blocking the core's intent polling.
    fn permission_future<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> futures::future::BoxFuture<'a, PermissionDecision> {
        Box::pin(futures::future::ready(self.permission(request)))
    }
}

#[derive(Debug)]
pub(super) struct SeatApprover {
    pub(super) seat: Arc<dyn InteractiveSeat>,
}

impl HumanApprover for SeatApprover {
    fn decide<'a>(
        &'a self,
        _item: ItemId,
        request: &'a PermissionRequest,
    ) -> futures::future::BoxFuture<'a, PermissionDecision> {
        self.seat.permission_future(request)
    }
}

/// The standing answer of an owner who set `deny_prompts`: nobody watches the
/// terminal, so every permission question is refused at once. The refusal
/// carries words, which makes it the tool result the model reads; the turn
/// continues instead of waiting on a prompt.
#[derive(Debug)]
struct PromptDenier;

const DENIED_WITHOUT_PROMPT: &str = "nobody is available to approve it, because this \
     session denies every call that needs a permission prompt (deny_prompts). Continue \
     without this call, or report the authority you need.";

impl HumanApprover for PromptDenier {
    fn decide<'a>(
        &'a self,
        _item: ItemId,
        _request: &'a PermissionRequest,
    ) -> futures::future::BoxFuture<'a, PermissionDecision> {
        Box::pin(futures::future::ready(
            PermissionDecision::DenyWithInstruction {
                instruction: DENIED_WITHOUT_PROMPT.to_owned(),
            },
        ))
    }
}

/// The final permission party of an interactive seat: the human, or the
/// owner's `deny_prompts` answer in place of the prompt. `None` without a human.
pub(super) fn seat_human(
    seat: &Arc<dyn InteractiveSeat>,
    deny_prompts: bool,
) -> Option<Arc<dyn HumanApprover>> {
    if !seat.has_human() {
        return None;
    }
    Some(if deny_prompts {
        Arc::new(PromptDenier)
    } else {
        Arc::new(SeatApprover {
            seat: Arc::clone(seat),
        })
    })
}

/// The native terminal's session-only path authority. Command permission and
/// sandbox widening use the same terminal owner but retain separate decisions.
pub(super) struct SeatSandboxGrants {
    seat: Arc<dyn InteractiveSeat>,
    grants: Mutex<Vec<ego_core::permit::SandboxGrant>>,
    /// Grants a person kept for this workspace, shared with ACP sessions.
    store: Option<(Arc<dyn ego_acp::sandbox::SandboxGrantStore>, PathBuf)>,
    /// Deny rules in force at session start, consulted before asking.
    rules: ego_core::config::Ruleset,
    /// Paths the person kept blocked during this session.
    blocked: Mutex<Vec<ego_core::permit::SandboxGrant>>,
}

impl SeatSandboxGrants {
    /// `None` without a human, and with `deny_prompts`: the refusal then stays
    /// the command's result instead of a question nobody answers.
    pub(super) fn for_seat(
        seat: Arc<dyn InteractiveSeat>,
        store: Option<(Arc<dyn ego_acp::sandbox::SandboxGrantStore>, PathBuf)>,
        rules: ego_core::config::Ruleset,
        deny_prompts: bool,
    ) -> Option<Arc<dyn ego_core::permit::SandboxGrantPort>> {
        (seat.has_human() && !deny_prompts).then(|| {
            Arc::new(Self {
                seat,
                grants: Mutex::new(Vec::new()),
                store,
                rules,
                blocked: Mutex::new(Vec::new()),
            }) as Arc<dyn ego_core::permit::SandboxGrantPort>
        })
    }

    /// Kept blocked, by a stored deny rule or by an answer earlier in this session.
    fn kept_blocked(&self, grant: &ego_core::permit::SandboxGrant) -> bool {
        let workspace = self
            .store
            .as_ref()
            .map_or(std::path::Path::new("/"), |(_, workspace)| {
                workspace.as_path()
            });
        self.blocked
            .lock()
            .expect("session blocked sandbox paths")
            .iter()
            .any(|blocked| blocked.covers(&grant.path, grant.access))
            || self.rules.denies_sandbox(grant, workspace)
    }

    fn keep_for_session(&self, grant: &ego_core::permit::SandboxGrant) {
        let mut grants = self.grants.lock().expect("session sandbox grants");
        if !grants.contains(grant) {
            grants.push(grant.clone());
        }
    }
}

impl ego_core::permit::SandboxGrantPort for SeatSandboxGrants {
    fn granted(&self) -> Vec<ego_core::permit::SandboxGrant> {
        let mut granted = self
            .store
            .as_ref()
            .map(|(store, workspace)| store.load(workspace))
            .unwrap_or_default();
        for grant in self.grants.lock().expect("session sandbox grants").iter() {
            if !granted.contains(grant) {
                granted.push(grant.clone());
            }
        }
        granted
    }

    fn ask<'a>(
        &'a self,
        grant: &'a ego_core::permit::SandboxGrant,
        command: &'a str,
    ) -> futures::future::BoxFuture<'a, ego_core::permit::RefusalAnswer> {
        use ego_core::permit::RefusalAnswer;
        Box::pin(async move {
            if self.kept_blocked(grant) {
                return RefusalAnswer::Deny;
            }
            let request = PermissionRequest::sandbox_grant(grant, command);
            match self.seat.permission_future(&request).await {
                PermissionDecision::Allow => {
                    self.keep_for_session(grant);
                    RefusalAnswer::Session
                }
                PermissionDecision::AllowAlways if self.store.is_some() => RefusalAnswer::Always,
                PermissionDecision::DenyAlways => {
                    self.blocked
                        .lock()
                        .expect("session blocked sandbox paths")
                        .push(grant.clone());
                    RefusalAnswer::Deny
                }
                _ => RefusalAnswer::Deny,
            }
        })
    }
}

/// Only the interactive driver which owns this process can consume this slot.
pub(super) type RootTransitionSlot = Arc<Mutex<Option<ego_core::turn::RootTransitionApproval>>>;

pub(super) fn take_root_transition(
    slot: &RootTransitionSlot,
    continuation: Option<TurnId>,
) -> Result<Option<ego_core::turn::RootTransitionApproval>, String> {
    let approval = slot
        .lock()
        .map_err(|_| "root transition slot poisoned")?
        .take();
    if continuation.is_some() && approval.is_none() {
        return Err("same-turn root continuation has no measured approval".into());
    }
    Ok(approval)
}

pub(super) struct RootTransitionContext {
    pub(super) confinement: RunConfinement,
    pub(super) previous: ExecObservation,
    pub(super) session_roots: Vec<ego_core::permit::ProfileRoot>,
    pub(super) evidence: SessionEvidenceStore,
    pub(super) cancel: CancelToken,
    pub(super) deadline: Option<std::time::Instant>,
    pub(super) slot: RootTransitionSlot,
}

struct MeasuredRootChoice {
    observation: ExecObservation,
    approval: ego_core::turn::RootTransitionApproval,
}

struct RootChoiceMeasurements {
    session: MeasuredRootChoice,
    permanent: MeasuredRootChoice,
}

impl RootTransitionContext {
    async fn measure(
        &self,
        session: SessionId,
        item: ItemId,
        request: &ego_core::turn::RootGrantRequest,
        proposed: &RunConfinement,
    ) -> Result<MeasuredRootChoice, String> {
        if self.cancel.is_cancelled() {
            return Err("root transition measurement cancelled".into());
        }
        let (_, observation, _) = effect_scope(
            proposed,
            &self.evidence,
            &self.cancel,
            self.deadline,
            None,
            None,
        )
        .await
        .map_err(|error| error.exit_reason().to_owned())?;
        let approval = ego_core::turn::RootTransitionApproval::approved(
            session,
            item,
            request,
            self.previous.clone(),
            observation.clone(),
        )?;
        Ok(MeasuredRootChoice {
            observation,
            approval,
        })
    }
}

/// Reapply the durable session receipts just as fresh-process re-admission
/// does. A configured Permanent root precedes those receipts; a new Session
/// receipt follows them. Do not replace this assembly with set equivalence:
/// order is part of the measured policy digest.
fn root_choice_confinements(
    proposed: &Effective,
    confinement: &RunConfinement,
    session_roots: &[ego_core::permit::ProfileRoot],
    request: &ego_core::turn::RootGrantRequest,
) -> Result<(RunConfinement, RunConfinement), String> {
    let requested = ego_core::permit::ProfileRoot::new(request.path.clone(), request.access);
    let mut session_grants = session_roots.to_vec();
    if !session_grants.contains(&requested) {
        session_grants.push(requested.clone());
    }
    let mut session_effective = proposed.clone();
    let session = super::execution::assemble_session_roots(
        &mut session_effective,
        &session_grants,
        confinement.workspace(),
        |_| {},
    )
    .map_err(|error| error.0)?;
    if !session.contains(&requested) {
        return Err("session root is outside the final admitted ceiling".into());
    }
    let mut permanent_effective = proposed.clone();
    let mut permanent = super::execution::assemble_session_roots(
        &mut permanent_effective,
        session_roots,
        confinement.workspace(),
        |_| {},
    )
    .map_err(|error| error.0)?;
    if !permanent.contains(&requested) {
        return Err("permanent root is outside the final admitted ceiling".into());
    }
    // The real permanent publication adds a committed terminal receipt.
    for root in &mut permanent {
        if *root == requested {
            *root = root.clone().with_terminal_write();
        }
    }
    Ok((
        confinement.clone().with_roots(session),
        confinement.clone().with_roots(permanent),
    ))
}

/// The connected human seat may save a refused root.
pub(super) struct SeatRootGrants {
    pub(super) resume_turn: bool,
    pub(super) transition: Option<RootTransitionContext>,
    pub(super) elicitor: Arc<dyn Elicitor>,
    pub(super) effective: Effective,
    pub(super) workspace: CanonPath,
    pub(super) target: UserConfigTarget,
    pub(super) rule_writer: Option<Arc<dyn ego_acp::permission::PermissionRuleWriter>>,
    pub(super) store_root: PathBuf,
}

impl ego_core::turn::RootGrantPort for SeatRootGrants {
    fn supports_readmission(&self) -> bool {
        self.resume_turn
    }

    fn ask<'a>(
        &'a self,
        session: SessionId,
        item: ItemId,
        request: ego_core::turn::RootGrantRequest,
    ) -> futures::future::BoxFuture<'a, Result<ego_core::turn::RootGrantChoice, String>> {
        use ego_core::question::{QuestionMode, QuestionOption, QuestionRequest};
        use ego_core::turn::RootGrantChoice;
        Box::pin(async move {
            if let Some(context) = &self.transition {
                // An unanswered, denied or superseded question cannot retain trust.
                context
                    .slot
                    .lock()
                    .map_err(|_| "root transition slot poisoned")?
                    .take();
            }
            let path = ego_core::permit::resolve_root(
                request.path.as_path(),
                request.access,
                &ego_tools::fs::OsFs,
            )
            .map_err(|error| format!("root path changed or disappeared: {error}"))?;
            if path != request.path {
                return Err("root path changed since the refused operation".into());
            }
            let grant = ego_core::permit::SandboxGrant::new(
                path.clone(),
                match request.access {
                    ego_core::permit::RootAccess::Read => ego_core::permit::SandboxAccess::Read,
                    ego_core::permit::RootAccess::ReadWrite => {
                        ego_core::permit::SandboxAccess::Write
                    }
                },
            );
            if let Some(writer) = &self.rule_writer
                && writer.denies_sandbox(&grant, self.workspace.as_path())?
            {
                return Ok(RootGrantChoice::Deny);
            }
            let mut proposed = self.effective.clone();
            // A root removed from disk since admission (a deleted adjacent
            // checkout) has no reach left; it must not block this question.
            proposed.roots.retain(|root| {
                root.path != path.as_path()
                    && !(root.path.is_absolute() && matches!(root.path.try_exists(), Ok(false)))
            });
            proposed.roots.push(ego_core::permit::RootCfg {
                consent: None,
                path: path.as_path().to_path_buf(),
                access: request.access,
            });
            let admitted = super::profile_roots::admit(&proposed, &self.workspace)?;
            if !admitted
                .iter()
                .any(|root| root.path() == &path && root.access() == request.access)
            {
                return Err("root is outside the admitted ceiling".into());
            }
            let measured = if self.resume_turn && request.continuation.is_some() {
                let context = self
                    .transition
                    .as_ref()
                    .ok_or("same-turn root approval requires measured execution authority")?;
                let (session_policy, permanent_policy) = root_choice_confinements(
                    &proposed,
                    &context.confinement,
                    &context.session_roots,
                    &request,
                )?;
                let session_choice = context
                    .measure(session, item, &request, &session_policy)
                    .await?;
                let permanent = context
                    .measure(session, item, &request, &permanent_policy)
                    .await?;
                Some(RootChoiceMeasurements {
                    session: session_choice,
                    permanent,
                })
            } else {
                None
            };
            let shown_path = ego_core::permit::terminal_safe(&path.to_string());
            let access = match request.access {
                ego_core::permit::RootAccess::Read => "read-only",
                ego_core::permit::RootAccess::ReadWrite => "read and write",
            };
            let mut prompt = format!(
                "Allow {access} access?\nPath: {shown_path}\nRequested by: {}\nApplies after safe session re-admission; existing access stays unchanged. Native execution authorization and exec_paths are unchanged.",
                ego_core::permit::terminal_safe(&request.tool),
            );
            if path.as_path().is_dir() {
                prompt
                    .push_str("\nIncludes all files below this directory, including any credentials stored there.");
            }
            if request.access == ego_core::permit::RootAccess::ReadWrite {
                prompt.push_str("\nWrite access can change instruction files and Git controls used by later commands.");
                if cfg!(target_os = "linux") {
                    prompt.push_str("\nFor confined Linux child processes, a read-write root also carries executable reach under both Landlock and the namespace backend; native exec authorization remains separate.");
                }
            }
            if let Some(measured) = &measured {
                // Keep the exact root/access consequences beside the answer
                // controls even when complete measurements use scrollback.
                let root_summary = std::mem::take(&mut prompt);
                let context = self.transition.as_ref().expect("measured root context");
                if context.previous == measured.session.observation
                    && context.previous == measured.permanent.observation
                {
                    prompt.push_str("Measured execution policy is unchanged for both choices.");
                } else {
                    let previous = serde_json::to_string(&context.previous)
                        .map_err(|error| error.to_string())?;
                    prompt.push_str(&format!(
                        "This approval also covers only this pending action's measured execution transition for the selected duration.\nPrevious: {}",
                        ego_core::permit::terminal_safe(&previous),
                    ));
                    for (duration, choice) in [
                        ("Session", &measured.session),
                        ("Permanent", &measured.permanent),
                    ] {
                        let proposed = serde_json::to_string(&choice.observation)
                            .map_err(|error| error.to_string())?;
                        prompt.push_str(&format!(
                            "\nProposed: {duration}: {}",
                            ego_core::permit::terminal_safe(&proposed)
                        ));
                    }
                }
                prompt.push_str("\nRe-admission remeasures the proposed authority and matches both observations; any drift refuses. Saved roots do not grant reusable backend trust.\n\n");
                prompt.push_str(&root_summary);
            }
            let options = vec![
                QuestionOption::new("deny", "Keep blocked (remembered for this root)"),
                QuestionOption::new("session", "Session (this conversation only)"),
                QuestionOption::new("permanent", "Permanent (save for future sessions)"),
            ];
            let question = QuestionRequest::new(prompt, QuestionMode::Single, options, false);
            let choice = match self.elicitor.confirm_root(item, question).await {
                Ok(QuestionOutcome::Answered(QuestionAnswer::Single(answer))) => {
                    match answer.as_str() {
                        "session" => RootGrantChoice::Session,
                        "permanent" => RootGrantChoice::Permanent(ego_core::permit::RootConsent {
                            revision: SessionId::new(),
                            session,
                            store: self.store_root.clone(),
                        }),
                        "deny" => {
                            let writer = self
                                .rule_writer
                                .as_ref()
                                .ok_or("user rule store is unavailable")?;
                            let plan =
                                ego_core::config::rule_management::RememberPlan::for_sandbox_path(
                                    &grant,
                                )
                                .ok_or("this root cannot be named by a deny rule")?;
                            writer
                                .remember(ego_core::config::ManagedRuleEffect::Deny, plan)
                                .await?;
                            RootGrantChoice::Deny
                        }
                        _ => RootGrantChoice::Deny,
                    }
                }
                _ => RootGrantChoice::Deny,
            };
            if let Some(measured) = measured {
                let context = self.transition.as_ref().expect("measured root context");
                if context.cancel.is_cancelled()
                    || context
                        .deadline
                        .is_some_and(|deadline| std::time::Instant::now() >= deadline)
                {
                    return Err("root transition approval cancelled or expired".into());
                }
                let selected = match &choice {
                    RootGrantChoice::Session => Some(measured.session.approval),
                    RootGrantChoice::Permanent(_) => Some(measured.permanent.approval),
                    RootGrantChoice::Deny => None,
                };
                if let Some(approval) = selected {
                    *context
                        .slot
                        .lock()
                        .map_err(|_| "root transition slot poisoned")? = Some(approval);
                }
            }
            Ok(choice)
        })
    }

    fn publish<'a>(
        &'a self,
        consent: &'a ego_core::permit::PermanentRootConsent,
    ) -> futures::future::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let path = ego_core::permit::resolve_root(
                consent.root.path().as_path(),
                consent.root.access(),
                &ego_tools::fs::OsFs,
            )
            .map_err(|error| format!("root path changed or disappeared: {error}"))?;
            if &path != consent.root.path() {
                return Err("root path changed after terminal approval".into());
            }
            let result = crate::config::user_store()
                .map_err(|error| format!("{error:?}"))?
                .mutate(UserConfigMutation::GrantRoot {
                    target: self.target.clone(),
                    root: ego_core::permit::RootCfg {
                        path: path.as_path().to_path_buf(),
                        access: consent.root.access(),
                        consent: Some(consent.receipt.clone()),
                    },
                })
                .map_err(|error| error.to_string())?;
            if let UserConfigPublication::PublishedDurabilityUncertain { diagnostic } =
                result.publication
            {
                return Err(format!(
                    "root settings changed, but disk durability is uncertain: {diagnostic}"
                ));
            }
            Ok(())
        })
    }
}

/// The run-level limits, and the loop that applies them between turns.
///
/// Between turns is where they belong: `max_turns` fires *between* two
/// completed turns and is not honestly a cause of the preceding one, and a
/// token budget cannot interrupt a generation that is already paid for.
pub(super) struct Turns<'a> {
    pub(super) parent_cancel: CancelToken,
    pub(super) schema: Option<&'a schema::Schema>,
    pub(super) retries: u32,
    pub(super) max_turns: Option<u32>,
    pub(super) deadline: Option<std::time::Instant>,
}

enum TurnStart {
    Checkpoint,
    Queued,
    Prompt {
        text: String,
        images: Vec<ImageRef>,
        audio: Vec<ego_core::generator::audio::AudioRef>,
    },
    Resumed(ego_core::event::id::TurnId),
    Presented(ItemId),
    Answered {
        question: ItemId,
        answer: QuestionAnswer,
    },
    Skill(AdmittedSkillOperation),
    McpContext(repl::actions::McpContextCommand),
}

impl Turns<'_> {
    /// Continue prompts already durably accepted by this run's sole receiver.
    pub(super) async fn drive_queued<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, TurnStart::Queued, cancel, diagnostics)
            .await
    }

    pub(super) async fn drive_with_images<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        first: &str,
        images: Vec<ImageRef>,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_with_media(loop_, first, images, Vec::new(), cancel, diagnostics)
            .await
    }

    pub(super) async fn drive_with_media<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        first: &str,
        images: Vec<ImageRef>,
        audio: Vec<ego_core::generator::audio::AudioRef>,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(
            loop_,
            TurnStart::Prompt {
                text: first.to_owned(),
                images,
                audio,
            },
            cancel,
            diagnostics,
        )
        .await
    }

    pub(super) async fn drive_admitted_skill<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        operation: AdmittedSkillOperation,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, TurnStart::Skill(operation), cancel, diagnostics)
            .await
    }

    pub(super) async fn drive_mcp_context<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        command: repl::actions::McpContextCommand,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, TurnStart::McpContext(command), cancel, diagnostics)
            .await
    }

    pub(super) async fn drive_user_shell<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        command: String,
        environment: ego_core::permit::EnvPolicy,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        let argv = if cfg!(windows) {
            vec!["cmd.exe", "/C", command.as_str()]
        } else {
            vec!["/bin/sh", "-c", command.as_str()]
        };
        let tool: Arc<dyn ego_core::permit::Tool> = Arc::new(ego_core::permit::Typed(
            ego_tools::tools::ExecTool::with_environment(environment),
        ));
        match self
            .one_running(
                Box::pin(loop_.run_user_shell(serde_json::json!({ "argv": argv }), tool)),
                cancel,
            )
            .await
        {
            Ok(outcome) => {
                let termination =
                    RunTermination::after_outcome(&outcome).unwrap_or(RunTermination::Done);
                (termination, self.answer(&loop_.state()))
            }
            Err(termination) => {
                diagnostics.note(&format!(
                    "shell escape failed: {}",
                    termination.exit_reason()
                ));
                (termination, self.answer(&loop_.state()))
            }
        }
    }

    pub(super) async fn drive_checkpoint<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, TurnStart::Checkpoint, cancel, diagnostics)
            .await
    }

    pub(super) async fn drive_resumed<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        turn: ego_core::event::id::TurnId,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, TurnStart::Resumed(turn), cancel, diagnostics)
            .await
    }

    pub(super) async fn drive_answered<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        question: ItemId,
        answer: QuestionAnswer,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(
            loop_,
            TurnStart::Answered { question, answer },
            cancel,
            diagnostics,
        )
        .await
    }

    pub(super) async fn drive_presented<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        question: ItemId,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, TurnStart::Presented(question), cancel, diagnostics)
            .await
    }

    async fn drive_from<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        mut next: TurnStart,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        let mut taken: u32 = 0;
        let mut left = self.retries;

        loop {
            if self.max_turns.is_some_and(|max| taken >= max) {
                return (RunTermination::MaxTurns, self.answer(&loop_.state()));
            }
            let before_turns = loop_.state().turns.len();
            loop_.set_turn_allowance(self.max_turns.map(|max| max.saturating_sub(taken)));
            let outcome = match next {
                TurnStart::Queued => {
                    self.one_running(
                        Box::pin(async {
                            loop_.run_queued_turn().await.map(|outcome| {
                                outcome
                                    .expect("queued drive starts only with durable pending input")
                            })
                        }),
                        cancel,
                    )
                    .await
                }
                TurnStart::Checkpoint => {
                    self.one_running(Box::pin(loop_.continue_checkpoint()), cancel)
                        .await
                }
                TurnStart::Prompt {
                    text,
                    images,
                    audio,
                } => self.one(loop_, &text, images, audio, cancel).await,
                TurnStart::Resumed(turn) => self.one_resumed(loop_, turn, cancel).await,
                TurnStart::Presented(question) => {
                    self.one_running(Box::pin(loop_.present_foreign_question(question)), cancel)
                        .await
                }
                TurnStart::Answered { question, answer } => {
                    self.one_answered(loop_, question, answer, cancel).await
                }
                TurnStart::Skill(operation) => self.one_skill(loop_, operation, cancel).await,
                TurnStart::McpContext(command) => {
                    self.one_running(
                        Box::pin(loop_.run_admitted_context(
                            command.catalogue,
                            command.name,
                            command.parameters,
                        )),
                        cancel,
                    )
                    .await
                }
            };
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(termination) => return (termination, self.answer(&loop_.state())),
            };
            if loop_.root_readmission().is_some() {
                // Not an ACP hold, a failed task, or a completed turn. The
                // native driver replaces the epoch before the next generation.
                return (RunTermination::Done, self.answer(&loop_.state()));
            }
            taken = taken.saturating_add(
                u32::try_from(loop_.state().turns.len().saturating_sub(before_turns))
                    .unwrap_or(u32::MAX)
                    .max(1),
            );
            if loop_
                .state()
                .goal
                .as_ref()
                .and_then(|goal| goal.check_stop.as_deref())
                == Some("run max-turns reached")
            {
                return (RunTermination::MaxTurns, self.answer(&loop_.state()));
            }

            // A requested hold was durably recorded by the turn boundary;
            // pending input must not hide that control acknowledgement.
            if outcome.exit_reason != ego_core::event::domain::ExitReason::Paused
                && let Some(pending) = loop_.state().pending_question()
            {
                return (
                    RunTermination::QuestionRequired {
                        question: Box::new(QuestionReport::from_request(
                            pending.item_id(),
                            pending.target(),
                            pending.request(),
                        )),
                    },
                    self.answer(&loop_.state()),
                );
            }

            if let Some(skill) = outcome.skill.as_ref() {
                return match loop_.materialize_skill_outcome(skill) {
                    Ok((true, text)) => (RunTermination::Done, Value::String(text)),
                    Ok((false, diagnostic)) => {
                        diagnostics.note(&diagnostic);
                        (RunTermination::TaskFailed, Value::String(diagnostic))
                    }
                    Err(error) => (
                        RunTermination::Failed {
                            message: error.to_string(),
                        },
                        Value::String(error.to_string()),
                    ),
                };
            }

            if let Some(termination) = RunTermination::after_outcome(&outcome) {
                return (termination, self.answer(&loop_.state()));
            }

            // Durable steering owns the next provider request. A schema
            // repair is another user turn, so it must wait until the outer
            // interactive coordinator has replayed the accepted FIFO through
            // a fresh TurnLoop and cancellation token.
            if !loop_.state().unacknowledged_prompts().is_empty() {
                return (RunTermination::Done, self.answer(&loop_.state()));
            }

            let answer = last_assistant_message(&loop_.state());
            let Some(schema) = self.schema else {
                return (RunTermination::Done, Value::String(answer));
            };
            match schema.check(&answer) {
                Ok(document) => return (RunTermination::Done, document),
                Err(invalid) => {
                    diagnostics.note(&format!("the answer did not validate: {invalid}"));
                    if left == 0 {
                        return (RunTermination::TaskFailed, Value::String(answer));
                    }
                    left -= 1;
                    next = TurnStart::Prompt {
                        text: schema::repair_prompt(&invalid),
                        images: Vec::new(),
                        audio: Vec::new(),
                    };
                }
            }
        }
    }

    /// One turn, watched by the two things only a process can see: a signal,
    /// and a deadline.
    ///
    /// The cancellation is cooperative first and bounded second. The token is
    /// set so the loop unwinds cleanly — settling its attempt, writing
    /// `TurnCompleted` — and only if it has not finished within [`GRACE`] does
    /// the run stop waiting and report from the cause. Dropping the future
    /// immediately would abandon an unsettled reservation; waiting forever
    /// would make `--timeout` advisory.
    async fn one<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        prompt: &str,
        images: Vec<ImageRef>,
        audio: Vec<ego_core::generator::audio::AudioRef>,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination> {
        self.one_running(
            Box::pin(loop_.run_with_media(prompt, images, audio)),
            cancel,
        )
        .await
    }

    async fn one_resumed<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        turn: ego_core::event::id::TurnId,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination> {
        self.one_running(Box::pin(loop_.resume_turn(turn)), cancel)
            .await
    }

    async fn one_answered<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        question: ItemId,
        answer: QuestionAnswer,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination> {
        self.one_running(Box::pin(loop_.resume_question(question, answer)), cancel)
            .await
    }

    async fn one_skill<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        operation: AdmittedSkillOperation,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination> {
        self.one_running(Box::pin(loop_.run_admitted_skill(operation)), cancel)
            .await
    }

    async fn one_running<F>(
        &self,
        running: F,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination>
    where
        F: Future<Output = Result<TurnOutcome, TurnAbandoned>>,
    {
        let (outcome, stopped) = {
            let mut running = Box::pin(running);
            let mut stopped = false;
            loop {
                if stopped {
                    tokio::select! {
                        done = &mut running => break (Some(done), stopped),
                        () = tokio::time::sleep(GRACE) => break (None, stopped),
                    }
                }
                tokio::select! {
                    biased;
                    cause = self.parent_cancel.cancelled() => {
                        cancel.cancel(cause);
                        stopped = true;
                    }
                    _ = cancel.cancelled() => {
                        stopped = true;
                    }
                    cause = stop_signal(self.deadline) => {
                        cancel.cancel(cause);
                        stopped = true;
                    }
                    done = &mut running => {
                        // A provider can observe the shared deadline before
                        // stop_signal is polled again. Only successful completion
                        // is authoritative over that deadline.
                        if !matches!(&done, Ok(outcome) if outcome.ok)
                            && self.deadline.is_some_and(|deadline| Instant::now() >= deadline)
                        {
                            cancel.cancel(CancelCause::Timeout);
                            stopped = true;
                        }
                        break (Some(done), stopped);
                    }
                }
            }
        };
        // The core appends TurnCompleted before its cleanup tail. A deadline
        // observed in that tail cannot rewrite an authoritative completion.
        // Still cancel pending work and bound cleanup by GRACE above.
        if stopped && !matches!(&outcome, Some(Ok(outcome)) if outcome.ok) {
            return Err(cancelled_termination(cancel));
        }
        match outcome {
            Some(Ok(outcome)) => Ok(outcome),
            Some(Err(abandoned)) => Err(RunTermination::Failed {
                message: format!("the session log could not be written: {}", abandoned.cause),
            }),
            None => Err(cancelled_termination(cancel)),
        }
    }

    /// The last assistant message, as `CLI.md` §4.1's `result`.
    fn answer(&self, state: &SessionState) -> Value {
        Value::String(last_assistant_message(state))
    }
}

/// Whichever of a signal or the deadline arrives first, and who it was.
///
/// A `SIGINT` and an expired `--timeout` are different exits (130 and 124), so
/// this returns the cause rather than the fact — and `CancelToken` keeps the
/// first one, so a deadline that expires a moment after the user pressed
/// Ctrl-C does not rewrite what happened.
pub async fn stop_signal(deadline: Option<std::time::Instant>) -> CancelCause {
    let deadline = async {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
            // Nothing to wait for; never resolves, so `select!` ignores it.
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        cause = process_signal() => cause,
        () = deadline => CancelCause::Timeout,
    }
}

#[cfg(unix)]
async fn process_signal() -> CancelCause {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a process may always listen for SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => CancelCause::User,
        _ = term.recv() => CancelCause::Host,
    }
}

#[cfg(windows)]
async fn process_signal() -> CancelCause {
    tokio::signal::ctrl_c()
        .await
        .expect("a process may always listen for Ctrl-C");
    CancelCause::User
}

/// The text of the last assistant message in the session.
///
/// Read from `SessionState` rather than accumulated while streaming: the state
/// is the fold of the durable log, so what is reported is what was written
/// down. An empty string when the run produced no assistant message — §4.1's
/// `result` is a string, and a run that said nothing said the empty thing.
pub(super) fn last_assistant_message(state: &SessionState) -> String {
    state
        .items
        .values()
        .filter(|item| item.kind == ItemKind::Assistant)
        .max_by_key(|item| item.seq)
        .map(|item| item.text.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use ego_core::event::domain::ExitReason;

    fn outcome(ok: bool, exit_reason: ExitReason) -> TurnOutcome {
        TurnOutcome {
            turn: TurnId::new(),
            ok,
            exit_reason,
            denied: None,
            skill: None,
            provider_failure: None,
            guard_failure: None,
            usage_error: None,
        }
    }

    // Catches: a completed turn is rewritten as timeout when synchronous
    // completion cleanup crosses the deadline before returning its outcome.
    #[tokio::test]
    async fn completed_turn_is_not_rewritten_by_post_completion_deadline() {
        let turns = Turns {
            parent_cancel: CancelToken::new(),
            schema: None,
            retries: 0,
            max_turns: None,
            deadline: Some(Instant::now() + Duration::from_millis(20)),
        };
        let cancel = CancelToken::new();
        let result = turns
            .one_running(
                async {
                    let completed = outcome(true, ExitReason::Completed);
                    // Models the synchronous journal/hook tail after TurnCompleted.
                    std::thread::sleep(Duration::from_millis(40));
                    Ok(completed)
                },
                &cancel,
            )
            .await;
        assert!(result.unwrap().ok);
        assert!(!cancel.is_cancelled(), "completion does not mint a timeout");
    }

    // Catches: the stop branch discards an already-completed outcome returned
    // during cooperative cleanup, contradicting the durable successful turn.
    #[tokio::test]
    async fn completed_turn_survives_deadline_during_async_cleanup() {
        let turns = Turns {
            parent_cancel: CancelToken::new(),
            schema: None,
            retries: 0,
            max_turns: None,
            deadline: Some(Instant::now() + Duration::from_millis(20)),
        };
        let cancel = CancelToken::new();
        let result = turns
            .one_running(
                async {
                    let completed = outcome(true, ExitReason::Completed);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    Ok(completed)
                },
                &cancel,
            )
            .await;
        assert!(result.unwrap().ok);
    }

    // Catches: completion precedence accidentally waives a real running-turn timeout.
    #[tokio::test]
    async fn running_turn_still_times_out() {
        let turns = Turns {
            parent_cancel: CancelToken::new(),
            schema: None,
            retries: 0,
            max_turns: None,
            deadline: Some(Instant::now() + Duration::from_millis(20)),
        };
        let cancel = CancelToken::new();
        let result = turns
            .one_running(
                async {
                    while !cancel.is_cancelled() {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    Ok(outcome(false, ExitReason::Cancelled(CancelCause::Timeout)))
                },
                &cancel,
            )
            .await;
        assert!(matches!(result, Err(RunTermination::TimedOut)));
    }
}

#[cfg(test)]
mod root_grant_ceiling_tests {
    use super::*;
    use ego_core::permit::{RootAccess, RootCfg};
    use ego_core::turn::{RootGrantChoice, RootGrantPort, RootGrantRequest};
    use std::sync::Mutex;

    /// Answers every question with Session and keeps the prompts it saw.
    #[derive(Default)]
    struct Owner(Mutex<Vec<String>>);

    impl Elicitor for Owner {
        fn elicit<'a>(
            &'a self,
            _question: ItemId,
            request: ego_core::question::QuestionRequest,
        ) -> futures::future::BoxFuture<
            'a,
            Result<QuestionOutcome, ego_core::question::ElicitorError>,
        > {
            self.0.lock().unwrap().push(request.prompt);
            Box::pin(async {
                Ok(QuestionOutcome::Answered(QuestionAnswer::Single(
                    "session".into(),
                )))
            })
        }
    }

    // Catches: the native same-turn host silently falling back to an
    // unmeasured root-only answer when its measurement binding is missing.
    #[tokio::test]
    async fn same_turn_root_question_refuses_missing_measurement_before_asking() {
        let tree = tempfile::tempdir().unwrap();
        let workspace = tree.path().join("workspace");
        let outside = tree.path().join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let owner = Arc::new(Owner::default());
        let seat = SeatRootGrants {
            resume_turn: true,
            transition: None,
            elicitor: owner.clone(),
            effective: ego_core::config::resolve(&[], Trust::Untrusted, None).unwrap(),
            workspace: canon(&workspace),
            target: UserConfigTarget::Base,
            rule_writer: None,
            store_root: tree.path().join("store"),
        };
        let answer = seat
            .ask(
                SessionId::new(),
                ItemId::new(),
                RootGrantRequest {
                    continuation: Some(TurnId::new()),
                    tool: "read".into(),
                    operation: ego_core::permit::OpName::Read,
                    path: canon(&outside),
                    access: RootAccess::Read,
                },
            )
            .await;
        assert!(
            answer
                .unwrap_err()
                .contains("requires measured execution authority")
        );
        assert!(
            owner.0.lock().unwrap().is_empty(),
            "an unmeasured question reached the owner"
        );
    }

    // Catches: a root refusal asks again after the owner kept it blocked.
    #[tokio::test]
    async fn root_keep_blocked_survives_a_new_seat() {
        struct DenyOwner(std::sync::atomic::AtomicUsize);
        impl Elicitor for DenyOwner {
            fn elicit<'a>(
                &'a self,
                _: ItemId,
                request: ego_core::question::QuestionRequest,
            ) -> futures::future::BoxFuture<
                'a,
                Result<QuestionOutcome, ego_core::question::ElicitorError>,
            > {
                let deny = request
                    .options
                    .iter()
                    .find(|option| option.id == "deny")
                    .unwrap();
                assert_eq!(deny.label, "Keep blocked (remembered for this root)");
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async {
                    Ok(QuestionOutcome::Answered(QuestionAnswer::Single(
                        "deny".into(),
                    )))
                })
            }
        }
        let tree = tempfile::tempdir().unwrap();
        let workspace = tree.path().join("workspace");
        let outside = tree.path().join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let owner = Arc::new(DenyOwner(std::sync::atomic::AtomicUsize::new(0)));
        let writer = Arc::new(crate::acp_permission::CliAcpPermissionRules::new(
            tree.path().join("config.toml"),
            UserConfigTarget::Base,
        ));
        let make = || SeatRootGrants {
            resume_turn: false,
            transition: None,
            elicitor: owner.clone(),
            effective: ego_core::config::resolve(&[], ego_core::config::Trust::Untrusted, None)
                .unwrap(),
            workspace: canon(&workspace),
            target: UserConfigTarget::Base,
            rule_writer: Some(writer.clone()),
            store_root: tree.path().join("store"),
        };
        let request = || RootGrantRequest {
            continuation: None,
            tool: "write".into(),
            operation: ego_core::permit::OpName::Write,
            path: canon(&outside),
            access: RootAccess::ReadWrite,
        };
        assert_eq!(
            make()
                .ask(SessionId::new(), ItemId::new(), request())
                .await
                .unwrap(),
            RootGrantChoice::Deny
        );
        assert_eq!(
            make()
                .ask(SessionId::new(), ItemId::new(), request())
                .await
                .unwrap(),
            RootGrantChoice::Deny
        );
        assert_eq!(owner.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // Catches: Keep blocked saves only to disk, so the next yolo/off capture
    // authorizes the same external write without asking the root seat.
    #[tokio::test]
    async fn root_keep_blocked_binds_next_yolo_capture_for_base_and_profile() {
        use ego_core::permit::{Classification, PolicyDenied};
        use ego_core::turn::GenerationPolicySource;

        struct KeepBlocked(std::sync::atomic::AtomicUsize);
        impl Elicitor for KeepBlocked {
            fn elicit<'a>(
                &'a self,
                _: ItemId,
                _: ego_core::question::QuestionRequest,
            ) -> futures::future::BoxFuture<
                'a,
                Result<QuestionOutcome, ego_core::question::ElicitorError>,
            > {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async {
                    Ok(QuestionOutcome::Answered(QuestionAnswer::Single(
                        "deny".into(),
                    )))
                })
            }
        }
        struct NoAuto;
        impl PreflightPort for NoAuto {
            fn validate_auto(&self) -> Result<(), PolicyControlError> {
                unreachable!()
            }
        }

        let profile =
            UserConfigTarget::Profile(ego_core::config::ProfileName::parse("work").unwrap());
        for (target, inherited) in [
            (UserConfigTarget::Base, false),
            (profile.clone(), false),
            (profile, true),
        ] {
            let tree = tempfile::tempdir().unwrap();
            let workspace = tree.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let external = tree.path().join("notes.md");
            std::fs::write(&external, b"original").unwrap();
            let captured = tree.path().join("captured.md");
            let administered = tree.path().join("administered.md");
            std::fs::write(&captured, b"captured").unwrap();
            std::fs::write(&administered, b"administered").unwrap();
            let path = tree.path().join("config.toml");
            let table = if target == UserConfigTarget::Base || inherited {
                "permissions"
            } else {
                "profile.work.permissions"
            };
            let mut user_text = format!(
                "[{table}]\ndeny=[{:?}]\n",
                format!(
                    "write({})",
                    ego_core::permit::rule::PathPattern::from_path(
                        canon(&captured).as_path(),
                        false
                    )
                    .unwrap()
                )
            );
            if inherited {
                user_text.push_str("\n[profile.work]\n");
            }
            std::fs::write(&path, &user_text).unwrap();
            let system_text = format!(
                "[permissions]\ndeny=[{:?}]\n",
                format!(
                    "write({})",
                    ego_core::permit::rule::PathPattern::from_path(
                        canon(&administered).as_path(),
                        false
                    )
                    .unwrap()
                )
            );
            let layers = vec![
                ConfigLayer::parse(Source::System, &system_text).unwrap(),
                ConfigLayer::parse(Source::User, &user_text).unwrap(),
            ];
            let selected_profile = match &target {
                UserConfigTarget::Base => None,
                UserConfigTarget::Profile(name) => Some((name.to_string(), Source::Flags)),
            };
            let effective = resolve_selected(
                &layers,
                Trust::Untrusted,
                selected_profile
                    .as_ref()
                    .map(|(name, source)| (name.as_str(), *source)),
            )
            .unwrap();
            let selection =
                GenerationPolicySelection::new(Mode::Default, effective.permissions.clone());
            let resolver = Arc::new(super::super::composition::FrozenRuleResolver::new(
                layers,
                Trust::Untrusted,
                selected_profile,
                selection.clone(),
            ));
            let writer = Arc::new(
                crate::acp_permission::CliAcpPermissionRules::new(path.clone(), target.clone())
                    .with_live_policy(selection.clone(), resolver.clone()),
            );
            let owner = Arc::new(KeepBlocked(std::sync::atomic::AtomicUsize::new(0)));
            let seat = SeatRootGrants {
                resume_turn: false,
                transition: None,
                elicitor: owner.clone(),
                effective,
                workspace: canon(&workspace),
                target: target.clone(),
                rule_writer: Some(writer),
                store_root: tree.path().join("store"),
            };
            assert_eq!(
                seat.ask(
                    SessionId::new(),
                    ItemId::new(),
                    RootGrantRequest {
                        continuation: None,
                        tool: "write".into(),
                        operation: ego_core::permit::OpName::Write,
                        path: canon(&external),
                        access: RootAccess::ReadWrite,
                    }
                )
                .await
                .unwrap(),
                RootGrantChoice::Deny
            );
            let store = Arc::new(ego_tools::config::OsUserConfigStore::new(
                Arc::new(ego_tools::store::OsIo),
                path,
            ));
            assert_eq!(store.rule_snapshot(target.clone()).unwrap().deny.len(), 2);
            if target != UserConfigTarget::Base {
                assert_eq!(
                    store
                        .rule_snapshot(UserConfigTarget::Base)
                        .unwrap()
                        .deny
                        .len(),
                    usize::from(inherited)
                );
            }
            let application =
                PolicyApplicationOwner::new(selection.clone(), store, Arc::new(NoAuto), resolver);
            application
                .apply(PolicyControlRequest::SetMode(Mode::Yolo))
                .unwrap();
            let confinement =
                RunConfinement::new(Confinement::Off, false, canon(&workspace)).unwrap();
            let source = CliGenerationPolicySource::new(selection, canon(&workspace), None)
                .with_read_perimeter(&confinement);
            let policy = source.capture().unwrap().policy();
            for (file, from) in [
                (&external, Source::User),
                (&captured, Source::User),
                (&administered, Source::System),
            ] {
                let call = PreparedCall::new(
                    "write",
                    Op::Write {
                        path: canon(file),
                        change: Change::Whole(b"changed".to_vec()),
                    },
                );
                assert!(
                    matches!(policy.classify(call), Classification::Settled(Err(PolicyDenied::Denied { from: actual, .. })) if actual == from)
                );
            }
            assert_eq!(std::fs::read(&external).unwrap(), b"original");
            assert_eq!(std::fs::read(&captured).unwrap(), b"captured");
            assert_eq!(std::fs::read(&administered).unwrap(), b"administered");
            assert_eq!(owner.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    fn canon(path: &std::path::Path) -> CanonPath {
        CanonPath::new(std::fs::canonicalize(path).unwrap())
    }

    // Catches: an owner approval admits a root outside the system ceiling,
    // or widens a read-only ceiling to read-write, because the seat asks the
    // owner before it checks the ceiling.
    #[tokio::test]
    async fn session_path_grant_cannot_widen_parent_or_bypass_system_ceiling() {
        let tree = tempfile::tempdir().unwrap();
        let workspace = tree.path().join("workspace");
        let allowed = tree.path().join("allowed");
        let child = allowed.join("child");
        let sibling = tree.path().join("sibling");
        for dir in [&workspace, &child, &sibling] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let mut effective =
            ego_core::config::resolve(&[], ego_core::config::Trust::Untrusted, None).unwrap();
        effective.root_ceilings = vec![vec![RootCfg {
            consent: None,
            path: canon(&allowed).as_path().to_path_buf(),
            access: RootAccess::Read,
        }]];
        let owner = Arc::new(Owner::default());
        let seat = SeatRootGrants {
            resume_turn: false,
            transition: None,
            elicitor: owner.clone(),
            effective,
            workspace: canon(&workspace),
            target: ego_core::config::UserConfigTarget::Base,
            rule_writer: None,
            store_root: tree.path().join("store"),
        };
        let ask = |path: &std::path::Path, access| {
            seat.ask(
                SessionId::new(),
                ItemId::new(),
                RootGrantRequest {
                    continuation: None,
                    tool: "read".into(),
                    operation: ego_core::permit::OpName::Read,
                    path: canon(path),
                    access,
                },
            )
        };

        let outside = ask(&sibling, RootAccess::Read).await;
        let parent = ask(tree.path(), RootAccess::Read).await;
        let widened = ask(&child, RootAccess::ReadWrite).await;
        assert!(outside.is_err(), "sibling outside the ceiling: {outside:?}");
        assert!(parent.is_err(), "parent of the ceiling: {parent:?}");
        assert!(
            widened.is_err(),
            "read-write under a read ceiling: {widened:?}"
        );
        assert!(owner.0.lock().unwrap().is_empty(), "the owner was asked");

        let inside = ask(&child, RootAccess::Read).await;
        assert_eq!(inside, Ok(RootGrantChoice::Session));
        let prompts = owner.0.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].contains("Allow read-only access?"),
            "{}",
            prompts[0]
        );
        assert!(
            prompts[0].contains("Native execution authorization and exec_paths are unchanged."),
            "{}",
            prompts[0]
        );
        assert!(
            prompts[0].contains("including any credentials stored there"),
            "{}",
            prompts[0]
        );
        assert!(!prompts[0].contains("Write access"), "{}", prompts[0]);
    }
}

#[cfg(test)]
mod root_transition_tests {
    use super::*;
    use ego_core::permit::{
        BackendIdentity, ExecutionPolicy, PolicyDigest, ProfileRoot, RootAccess,
    };
    use ego_core::turn::{RootGrantRequest, RootTransitionApproval};

    // Catches: measuring [session A, permanent B] before approval while the
    // actual unchanged re-admission assembles [permanent B, session A].
    #[test]
    fn root_choice_proposals_use_actual_session_restore_order_for_both_durations() {
        let tree = tempfile::tempdir().unwrap();
        let workspace = tree.path().join("workspace");
        let a = tree.path().join("session-a");
        let b = tree.path().join("requested-b");
        for path in [&workspace, &a, &b] {
            std::fs::create_dir(path).unwrap();
        }
        let canon = |path: &Path| CanonPath::new(std::fs::canonicalize(path).unwrap());
        let workspace = canon(&workspace);
        let a = canon(&a);
        let b = canon(&b);
        let grant_a = ProfileRoot::new(a.clone(), RootAccess::Read);
        let base = ego_core::config::resolve(&[], Trust::Untrusted, None).unwrap();
        let mut current = base.clone();
        let current_roots = super::super::execution::assemble_session_roots(
            &mut current,
            std::slice::from_ref(&grant_a),
            &workspace,
            |_| {},
        )
        .unwrap();
        let confinement = RunConfinement::new(Confinement::Workspace, false, workspace.clone())
            .unwrap()
            .with_roots(current_roots);
        let configured_b = ego_core::permit::RootCfg {
            path: b.as_path().to_path_buf(),
            access: RootAccess::Read,
            consent: None,
        };
        let mut proposed = current;
        proposed.roots.push(configured_b.clone());
        let request = RootGrantRequest {
            continuation: Some(TurnId::new()),
            tool: "read".into(),
            operation: ego_core::permit::OpName::Read,
            path: b.clone(),
            access: RootAccess::Read,
        };
        let (session, permanent) = root_choice_confinements(
            &proposed,
            &confinement,
            std::slice::from_ref(&grant_a),
            &request,
        )
        .unwrap();
        let paths = |policy: &RunConfinement| {
            policy
                .roots()
                .iter()
                .map(|root| root.path().clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&session), vec![a.clone(), b.clone()]);
        assert_eq!(paths(&permanent), vec![b.clone(), a.clone()]);
        // Fresh config contains B, not the process-restored session root A.
        let mut actual = base;
        actual.roots.push(configured_b);
        let actual = super::super::execution::assemble_session_roots(
            &mut actual,
            &[grant_a],
            &workspace,
            |_| {},
        )
        .unwrap();
        assert_eq!(permanent.roots(), actual.as_slice());
    }

    // Catches: displaying an approval which final session-root assembly
    // already rejects because durable receipts require conflicting access.
    #[test]
    fn root_choice_proposals_refuse_conflicting_durable_access_before_asking() {
        let tree = tempfile::tempdir().unwrap();
        let workspace = tree.path().join("workspace");
        let outside = tree.path().join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let workspace = CanonPath::new(std::fs::canonicalize(workspace).unwrap());
        let outside = CanonPath::new(std::fs::canonicalize(outside).unwrap());
        let old = ProfileRoot::new(outside.clone(), RootAccess::Read);
        let mut proposed = ego_core::config::resolve(&[], Trust::Untrusted, None).unwrap();
        proposed.roots.push(ego_core::permit::RootCfg {
            path: outside.as_path().to_path_buf(),
            access: RootAccess::ReadWrite,
            consent: None,
        });
        let request = RootGrantRequest {
            continuation: Some(TurnId::new()),
            tool: "write".into(),
            operation: ego_core::permit::OpName::Write,
            path: outside.clone(),
            access: RootAccess::ReadWrite,
        };
        let confinement = RunConfinement::new(Confinement::Workspace, false, workspace)
            .unwrap()
            .with_roots(vec![old.clone()]);
        assert!(
            root_choice_confinements(&proposed, &confinement, &[old], &request).is_err(),
            "the final assembly cannot honor both existing read and requested write receipts"
        );
    }

    // Catches: reusing the root answer on a second re-admission. This calls
    // the same native handoff boundary the driver uses, not Option directly.
    #[test]
    fn root_transition_handoff_consumes_approval_once_and_refuses_missing_receipt() {
        let request = RootGrantRequest {
            continuation: Some(TurnId::new()),
            tool: "read".into(),
            operation: ego_core::permit::OpName::Read,
            path: CanonPath::new("/outside/exact"),
            access: RootAccess::Read,
        };
        let root = ProfileRoot::new(request.path.clone(), request.access);
        let base = RunConfinement::new(
            Confinement::Workspace,
            false,
            CanonPath::new(crate::ai_test_support::native("/workspace")),
        )
        .unwrap();
        let observed = |policy: &RunConfinement, backend, digit: &str| {
            ExecObservation::new(
                BackendIdentity::new(backend, "1"),
                Confinement::Workspace.required_caps(),
                PolicyDigest::try_from(digit.repeat(64)).unwrap(),
            )
            .with_policy(ExecutionPolicy::capture(policy, Vec::new()))
        };
        let previous = observed(&base, "bwrap", "1");
        let proposed = observed(&base.with_roots(vec![root.clone()]), "landlock", "2");
        let session = SessionId::new();
        let item = ItemId::new();
        let slot: RootTransitionSlot = Arc::new(Mutex::new(Some(
            RootTransitionApproval::approved(
                session,
                item,
                &request,
                previous.clone(),
                proposed.clone(),
            )
            .unwrap(),
        )));
        let shared = Arc::clone(&slot);
        take_root_transition(&slot, Some(request.continuation.unwrap()))
            .unwrap()
            .unwrap()
            .consume(
                session,
                Some(request.continuation.unwrap()),
                Some((request.continuation.unwrap(), item)),
                &previous,
                &proposed,
                &[root],
            )
            .unwrap();
        assert!(take_root_transition(&shared, Some(request.continuation.unwrap())).is_err());
        assert!(
            take_root_transition(
                &RootTransitionSlot::default(),
                Some(request.continuation.unwrap())
            )
            .is_err()
        );
        assert!(take_root_transition(&shared, None).unwrap().is_none());
    }
}
