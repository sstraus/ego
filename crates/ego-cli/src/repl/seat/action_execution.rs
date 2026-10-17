use super::*;

pub(crate) fn render_action_help(registry: &ActionRegistry, width: usize) -> String {
    const SECTIONS: [(&str, &[ActionId]); 6] = [
        (
            "Discover",
            &[
                ActionId::Help,
                ActionId::Config,
                ActionId::Setup,
                ActionId::Doctor,
                ActionId::Version,
            ],
        ),
        (
            "Model",
            &[ActionId::Model, ActionId::Thinking, ActionId::Usage],
        ),
        (
            "Control",
            &[
                ActionId::Mode,
                ActionId::Plan,
                ActionId::Yolo,
                ActionId::Permissions,
                ActionId::Retry,
            ],
        ),
        (
            "Session",
            &[
                ActionId::Sessions,
                ActionId::New,
                ActionId::Fork,
                ActionId::Compact,
                ActionId::Refresh,
                ActionId::Name,
                ActionId::Goal,
                ActionId::Export,
            ],
        ),
        (
            "Integrations",
            &[
                ActionId::Mcp,
                ActionId::Tasks,
                ActionId::Tools,
                ActionId::Skills,
                ActionId::Hooks,
            ],
        ),
        (
            "Surface",
            &[
                ActionId::Statusline,
                ActionId::Clear,
                ActionId::Expand,
                ActionId::Quit,
            ],
        ),
    ];
    let render = |action: &crate::repl::actions::ActionRegistration| {
        let labels = registry.shortcut_labels(action.id());
        let shortcut = if labels.is_empty() {
            String::new()
        } else {
            format!(" [{}]", labels.join(", "))
        };
        match registry.availability(action.id()) {
            Availability::Disabled { owner } => format!(
                "/{}{shortcut} — {} [unavailable: {owner}]",
                action.name(),
                action.description_text()
            ),
            Availability::Busy => format!(
                "/{}{shortcut} — {} [busy]",
                action.name(),
                action.description_text()
            ),
            Availability::Enabled => format!(
                "  /{}{shortcut}  {}",
                action.name(),
                action.description_text()
            ),
        }
    };
    let limit = width.max(1);
    let mut logical = vec!["εgo commands".to_owned()];
    for (section, ids) in SECTIONS {
        logical.push(section.to_owned());
        logical.extend(
            registry
                .iter()
                .filter(|action| ids.contains(&action.id()))
                .map(&render),
        );
    }
    let capabilities = registry.skill_menu("");
    if !capabilities.is_empty() {
        logical.push("Skills and custom commands".to_owned());
        logical.extend(
            capabilities
                .into_iter()
                .map(|(name, description)| format!("  /{name}  {description}")),
        );
    }
    logical.extend(
        registry
            .mcp_menu("")
            .into_iter()
            .map(|(name, description)| format!("  /{name}  {description}")),
    );
    logical.push("Keys".to_owned());
    logical.extend(
        registry
            .keys()
            .into_iter()
            .map(|key| format!("  {}  {}", key.label, key.description)),
    );
    logical.push("─".repeat(limit));
    logical
        .into_iter()
        .flat_map(|row| crate::render::Line::plain(row).wrap(limit))
        .map(|row| row.text())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The banner's key line: every key the registry gives a hint, then the help
/// command, packed onto rows no wider than `width` without splitting a key.
pub(crate) fn render_key_hints(registry: &ActionRegistry, width: usize) -> String {
    const INDENT: &str = "     ";
    // A seat that has not measured a terminal reports 0, which means "wide
    // enough", not "narrower than any terminal".
    let width = if width == 0 { 80 } else { width.max(40) };
    let limit = width.saturating_sub(INDENT.len());
    let items = registry
        .keys()
        .into_iter()
        .filter(|key| !key.hint.is_empty())
        .map(|key| format!("{} {}", key.label, key.hint))
        .chain([format!("/{}", registry.help(ActionId::Help).name())]);
    let mut rows: Vec<String> = Vec::new();
    for item in items {
        match rows.last_mut() {
            Some(row) if crate::render::Line::plain(format!("{row} · {item}")).width() <= limit => {
                row.push_str(" · ");
                row.push_str(&item);
            }
            _ => rows.push(item),
        }
    }
    rows.iter()
        .map(|row| format!("{INDENT}{row}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Both submission paths enforce the same busy-command boundary.
fn busy_dispatch_refusal(dispatch: &Dispatch) -> Option<&'static str> {
    match dispatch {
        Dispatch::History(_) | Dispatch::Resume(_) => {
            Some("history is unavailable while a turn is running")
        }
        Dispatch::Skill(_) => Some("a skill cannot be invoked while a turn is running"),
        Dispatch::Skills(_) => Some("skill actions are unavailable while a turn is running"),
        Dispatch::Export(_) => Some("export is unavailable while a turn is running"),
        Dispatch::Fork(_) => Some("switching forks is unavailable while a turn is running"),
        Dispatch::McpContext(_) => {
            Some("MCP context commands are unavailable while a turn is running")
        }
        Dispatch::Mcp(_) => Some("MCP changes are unavailable while a turn is running"),
        Dispatch::Setup(_) => Some("setup is unavailable while a turn is running"),
        Dispatch::Hooks => Some("hook inspection is unavailable while a turn is running"),
        _ => None,
    }
}

fn refuse_busy_dispatch(status: &SharedStatus, dispatch: &Dispatch) -> bool {
    let Some(reason) = busy_dispatch_refusal(dispatch) else {
        return false;
    };
    status.lock().expect("surface status lock").notice = Some(reason.to_owned());
    repaint_status(status);
    true
}

fn submit_busy_line(
    editor: &mut Reedline,
    prompt: &dyn Prompt,
    phase: &SurfacePhase,
    status: &SharedStatus,
    ingress: &InteractiveIngress,
    drafts: &mut DraftStash,
    steer: bool,
) -> Option<(Line, bool)> {
    let buffer = editor.current_buffer_contents().to_string();
    let dispatch = {
        let status = status.lock().expect("surface status lock");
        status.actions.as_ref().map_or(Dispatch::Prompt, |actions| {
            actions
                .registry
                .classify(actions.registry.submit(buffer.clone(), status.origin))
        })
    };
    if refuse_busy_dispatch(status, &dispatch) {
        return None;
    }
    match dispatch {
        Dispatch::Action(action) => {
            let (handled, interrupted) =
                execute_action_and_modal(editor, phase, status, ingress, action, true);
            if let Some(interrupted) = interrupted {
                editor.run_edit_commands(&[EditCommand::Clear]);
                status.lock().expect("surface status lock").origin = InputOrigin::Typed;
                return Some((interrupted, false));
            }
            if handled {
                editor.run_edit_commands(&[EditCommand::Clear]);
                status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            }
            return None;
        }
        Dispatch::SessionMetadata(operation) => {
            route_session_metadata(status, operation, true);
            return None;
        }
        Dispatch::Config(arguments) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((Line::HostCommand(HostCommand::Config(arguments)), false));
        }
        Dispatch::Tasks(arguments) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((Line::HostCommand(HostCommand::Tasks(arguments)), false));
        }
        Dispatch::UnknownSlash(message) => {
            commit_to_transcript(status, &message);
            return None;
        }
        Dispatch::Refused { reason } => {
            commit_to_transcript(status, reason);
            return None;
        }
        Dispatch::History(_)
        | Dispatch::Resume(_)
        | Dispatch::Skill(_)
        | Dispatch::Skills(_)
        | Dispatch::Export(_)
        | Dispatch::Fork(_)
        | Dispatch::McpContext(_)
        | Dispatch::Mcp(_)
        | Dispatch::Setup(_)
        | Dispatch::Hooks => unreachable!("busy commands were refused before dispatch"),
        Dispatch::Prompt | Dispatch::Immediate | Dispatch::InteractiveIngress => {}
    }
    match futures::executor::block_on(ingress.submit(buffer.clone())) {
        Ok(queue) => {
            if editor.accept_buffer(prompt).is_err() {
                return Some((Line::Ended, true));
            }
            let mut current = status.lock().expect("surface status lock");
            current.queue = queue;
            current.origin = InputOrigin::Typed;
            if let Some(snapshot) = current.snapshot.as_mut() {
                snapshot.set_queue(queue);
            }
            current.notice = None;
            drop(current);
            if !restore_latest_draft(editor, status, drafts) {
                repaint_status(status);
            }
            // A steer is the submission plus the Esc path: the same
            // durable expedite, not a second delivery mechanism.
            if steer && request_interrupt(status, ingress) {
                return Some((Line::HostCommand(HostCommand::Interrupt), false));
            }
        }
        Err(ego_core::turn::ControlError::Closed) => {
            // Busy admission can close before the terminal observes idle.
            // Hand this submission to the next turn through normal input.
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            restore_latest_draft(editor, status, drafts);
            return Some((Line::Submitted(buffer), false));
        }
        Err(error) => {
            status.lock().expect("surface status lock").notice = Some(error.to_string());
        }
    }
    None
}

pub(super) fn surface_line(
    editor: &mut Reedline,
    prompt: &dyn Prompt,
    phase: &SurfacePhase,
    status: &SharedStatus,
    asked_for_image: &AtomicBool,
    ingress: &InteractiveIngress,
    drafts: &mut DraftStash,
) -> (Line, bool) {
    loop {
        if phase.get() == EditPhase::Idle {
            question::tell_host(status, |osc, _| {
                [
                    osc.state(crate::render::osc::State::Idle),
                    osc.progress(crate::render::osc::Progress::Cleared),
                ]
                .concat()
            });
        }
        if phase.get() == EditPhase::Busy {
            let notice = status.lock().expect("surface status lock").notice.take();
            if let Some(notice) = notice {
                // A host command suspends the live prompt in place. Retire it
                // before the notice is queued, including at the bottom row
                // where resuming the old footprint would scroll it into history.
                if editor.erase_last_prompt().is_err() {
                    return (Line::Ended, true);
                }
                commit_to_transcript(status, &format!("{}: {notice}", crate::DISPLAY_NAME));
            }
        }
        let signal = editor.read_line(prompt);
        // Reject blank input before Reedline commits it to history or clears
        // the composer, for both idle submission and busy queue admission.
        let signal = if matches!(
            &signal,
            Ok(reedline::Signal::HostCommand(command))
                if command == BUSY_SUBMIT_COMMAND || command == BUSY_STEER_COMMAND
        ) {
            let buffer = editor.current_buffer_contents().to_string();
            if buffer.trim().is_empty() {
                continue;
            }
            if phase.get() == EditPhase::Idle {
                editor
                    .accept_buffer(prompt)
                    .map(|()| reedline::Signal::Success(buffer))
            } else {
                signal
            }
        } else {
            signal
        };
        // Hold only the isolated PTY child at the real suspended-read boundary.
        #[cfg(test)]
        if asked_for_image.load(Ordering::Relaxed) && std::env::var_os("EGO_REGRESSION_HOLD").is_some() {
            std::fs::write("paste-inspected", "ready").unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !Path::new("paste-release").exists() {
                assert!(Instant::now() < deadline, "paste suspension release");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let failed = signal.is_err();
        let line = answer(asked_for_image.swap(false, Ordering::Relaxed), signal);
        if matches!(
            line,
            Line::Interrupted | Line::HostCommand(HostCommand::Interrupt)
        ) {
            let cancel = status
                .lock()
                .expect("surface status lock")
                .setup_cancel
                .clone();
            if let Some(cancel) = cancel {
                let _ = cancel.send(true);
                continue;
            }
        }
        if let Line::Submitted(text) = &line
            && phase.get() != EditPhase::Modal
        {
            let dispatch = {
                let mut current = status.lock().expect("surface status lock");
                let dispatch = current
                    .actions
                    .as_ref()
                    .map_or(Dispatch::Prompt, |actions| {
                        let submission = actions.registry.submit(text.clone(), current.origin);
                        actions.registry.classify(submission)
                    });
                current.origin = InputOrigin::Typed;
                dispatch
            };
            if phase.get() == EditPhase::Busy
                && !matches!(dispatch, Dispatch::Resume(_))
                && refuse_busy_dispatch(status, &dispatch)
            {
                continue;
            }
            match dispatch {
                Dispatch::Action(action) => {
                    let (handled, interrupted) = execute_action_and_modal(
                        editor,
                        phase,
                        status,
                        ingress,
                        action,
                        phase.get() == EditPhase::Busy,
                    );
                    if let Some(interrupted) = interrupted {
                        return (interrupted, false);
                    }
                    if !handled {
                        return (line, failed);
                    }
                    continue;
                }
                Dispatch::Resume(id) => {
                    return (Line::HostCommand(HostCommand::Resume(id)), failed);
                }
                Dispatch::History(query) => {
                    return (Line::HostCommand(HostCommand::History(query)), failed);
                }
                Dispatch::SessionMetadata(operation) => {
                    if let Some(line) =
                        route_session_metadata(status, operation, phase.get() == EditPhase::Busy)
                    {
                        return (line, failed);
                    }
                    continue;
                }
                Dispatch::Skill(operation) => {
                    return (Line::HostCommand(HostCommand::Skill(operation)), failed);
                }
                Dispatch::Skills(action) => {
                    return (Line::HostCommand(HostCommand::Skills(action)), failed);
                }
                Dispatch::Export(selection) => {
                    return (Line::HostCommand(HostCommand::Export(selection)), failed);
                }
                Dispatch::Fork(item) => {
                    return (Line::HostCommand(HostCommand::Fork(item)), failed);
                }
                Dispatch::Tasks(arguments) => {
                    return (Line::HostCommand(HostCommand::Tasks(arguments)), failed);
                }
                Dispatch::McpContext(command) => {
                    return (Line::HostCommand(HostCommand::McpContext(command)), failed);
                }
                Dispatch::Mcp(arguments) => {
                    return (Line::HostCommand(HostCommand::Mcp(arguments)), failed);
                }
                Dispatch::Setup(arguments) => {
                    return (Line::HostCommand(HostCommand::Setup(arguments)), failed);
                }
                Dispatch::Config(arguments) => {
                    return (Line::HostCommand(HostCommand::Config(arguments)), failed);
                }
                Dispatch::Hooks => {
                    return (Line::HostCommand(HostCommand::Hooks), failed);
                }
                Dispatch::UnknownSlash(message) => {
                    commit_to_transcript(status, &message);
                    continue;
                }
                Dispatch::Refused { reason } => {
                    commit_to_transcript(status, reason);
                    continue;
                }
                Dispatch::Prompt => {
                    editor.run_edit_commands(&[EditCommand::Clear]);
                    restore_latest_draft(editor, status, drafts);
                    return (line, failed);
                }
                Dispatch::Immediate | Dispatch::InteractiveIngress => return (line, failed),
            }
        }
        let Line::HostCommand(command) = line else {
            if matches!(line, Line::Interrupted) && phase.get() == EditPhase::Busy {
                match futures::executor::block_on(ingress.interrupt()) {
                    Ok(queue) => {
                        let mut current = status.lock().expect("surface status lock");
                        current.queue = queue;
                        if let Some(snapshot) = current.snapshot.as_mut() {
                            snapshot.set_queue(queue);
                        }
                        current.notice = Some("cancelling active work".to_string());
                        drop(current);
                        repaint_status(status);
                        return (Line::HostCommand(HostCommand::Interrupt), false);
                    }
                    Err(error) => {
                        status.lock().expect("surface status lock").notice =
                            Some(error.to_string());
                        repaint_status(status);
                        continue;
                    }
                }
            }
            if matches!(line, Line::Blank | Line::Interrupted) {
                status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            }
            return (line, failed);
        };

        if let HostCommand::Action(action) = &command {
            let (_, interrupted) = execute_action_and_modal(
                editor,
                phase,
                status,
                ingress,
                *action,
                phase.get() == EditPhase::Busy,
            );
            if let Some(interrupted) = interrupted {
                return (interrupted, false);
            }
            continue;
        }

        match command {
            HostCommand::ClipboardPaste | HostCommand::PastePath(_)
                if phase.get() == EditPhase::Busy =>
            {
                commit_to_transcript(
                    status,
                    "image paste is unavailable while source work is pending",
                );
            }
            HostCommand::Background => {
                let notice = if phase.get() == EditPhase::Busy {
                    match futures::executor::block_on(ingress.background()) {
                        Ok(id) => format!("background exec id: {id}"),
                        Err(error) => error.to_string(),
                    }
                } else {
                    "no running exec to background".into()
                };
                status.lock().expect("surface status lock").notice = Some(notice);
                repaint_status(status);
            }
            HostCommand::Draft => {
                let buffer = editor.current_buffer_contents().to_string();
                if buffer.is_empty() {
                    if restore_latest_draft(editor, status, drafts) {
                        continue;
                    }
                } else {
                    let origin = status.lock().expect("surface status lock").origin;
                    drafts.push_active(ComposerDraft {
                        text: buffer,
                        cursor: editor.current_insertion_point(),
                        origin,
                    });
                    editor.run_edit_commands(&[EditCommand::Clear]);
                    status.lock().expect("surface status lock").origin = InputOrigin::Typed;
                }
                let mut current = status.lock().expect("surface status lock");
                current.drafts = drafts.active_count();
                let draft_count = current.drafts;
                if let Some(snapshot) = current.snapshot.as_mut() {
                    snapshot.set_drafts(draft_count);
                }
                current.notice = None;
                drop(current);
                repaint_status(status);
            }
            HostCommand::BusyRecall => {
                match futures::executor::block_on(ingress.recall()) {
                    Ok(Some((text, queue))) => {
                        editor.run_edit_commands(&[EditCommand::InsertString(text)]);
                        let mut current = status.lock().expect("surface status lock");
                        current.queue = queue;
                        current.origin = InputOrigin::BracketedPaste;
                        current.notice = None;
                        if let Some(snapshot) = current.snapshot.as_mut() {
                            snapshot.set_queue(queue);
                        }
                    }
                    Ok(None) | Err(ego_core::turn::ControlError::Closed) => {}
                    Err(error) => {
                        status.lock().expect("surface status lock").notice =
                            Some(error.to_string());
                    }
                }
                repaint_status(status);
            }
            HostCommand::BusySubmit | HostCommand::BusySteer => {
                if let Some(outcome) = submit_busy_line(
                    editor,
                    prompt,
                    phase,
                    status,
                    ingress,
                    drafts,
                    command == HostCommand::BusySteer,
                ) {
                    return outcome;
                }
            }
            HostCommand::Interrupt => {
                if request_interrupt(status, ingress) {
                    return (Line::HostCommand(HostCommand::Interrupt), false);
                }
            }
            command => return (Line::HostCommand(command), failed),
        }

        // A suspended read retains the exact buffer and cursor. Starting it
        // again repaints the state changed by the local action above.
    }
}

/// Ask the coordinator to expedite the queue and cancel the running turn.
/// Returns whether it was accepted; a refusal is shown as the notice.
fn request_interrupt(status: &SharedStatus, ingress: &InteractiveIngress) -> bool {
    match futures::executor::block_on(ingress.interrupt()) {
        Ok(queue) => {
            let mut current = status.lock().expect("surface status lock");
            current.queue = queue;
            if let Some(snapshot) = current.snapshot.as_mut() {
                snapshot.set_queue(queue);
            }
            current.notice = None;
            drop(current);
            repaint_status(status);
            true
        }
        Err(error) => {
            status.lock().expect("surface status lock").notice = Some(error.to_string());
            false
        }
    }
}

fn restore_latest_draft(
    editor: &mut Reedline,
    status: &SharedStatus,
    drafts: &mut DraftStash,
) -> bool {
    if !editor.current_buffer_contents().is_empty() {
        return false;
    }
    let Some(saved) = drafts.pop_active() else {
        return false;
    };
    editor.run_edit_commands(&[
        EditCommand::InsertString(saved.text),
        EditCommand::MoveToPosition {
            position: saved.cursor,
            select: false,
        },
    ]);
    {
        let mut current = status.lock().expect("surface status lock");
        current.origin = saved.origin;
        current.drafts = drafts.active_count();
        let draft_count = current.drafts;
        if let Some(snapshot) = current.snapshot.as_mut() {
            snapshot.set_drafts(draft_count);
        }
        current.notice = None;
    }
    repaint_status(status);
    true
}

fn execute_action(
    status: &SharedStatus,
    ingress: &InteractiveIngress,
    action: ActionId,
    busy: bool,
) -> (bool, bool) {
    if action == ActionId::Retry {
        let notice = if busy {
            match futures::executor::block_on(ingress.retry_now()) {
                Ok(()) => "retrying provider now".to_owned(),
                Err(error) => error.to_string(),
            }
        } else {
            "no provider retry wait is active".to_owned()
        };
        status.lock().expect("surface status lock").notice = Some(notice);
        repaint_status(status);
        return (true, true);
    }
    let Some(actions) = status.lock().expect("surface status lock").actions.clone() else {
        return (false, false);
    };
    let displayed = actions.registry.snapshot();
    let Some(selection) = actions.registry.select(action, &displayed) else {
        return (false, false);
    };
    let current = actions.registry.snapshot();
    let dispatch = {
        let mut sink = actions.sink.lock().expect("local action sink lock");
        actions
            .registry
            .dispatch_to(selection, &current, busy, &mut *sink)
    };
    let (handled, completed, notice) = match dispatch {
        ActionDispatch::Local(LocalActionOutcome::Completed) => (true, true, None),
        ActionDispatch::Local(LocalActionOutcome::InFlight) => {
            (true, false, Some("action is already in flight".to_string()))
        }
        ActionDispatch::Local(LocalActionOutcome::Refused { reason })
        | ActionDispatch::Refused { reason } => (false, false, Some(reason.to_string())),
        ActionDispatch::Prompt(payload) => {
            let canonical = format!("/{}", actions.registry.help(payload.id).name());
            match futures::executor::block_on(ingress.submit(canonical)) {
                Ok(queue) => {
                    let mut current = status.lock().expect("surface status lock");
                    current.queue = queue;
                    if let Some(snapshot) = current.snapshot.as_mut() {
                        snapshot.set_queue(queue);
                    }
                    (true, true, None)
                }
                Err(error) => (false, false, Some(error.to_string())),
            }
        }
    };
    status.lock().expect("surface status lock").notice = notice;
    repaint_status(status);
    (handled, completed)
}

fn execute_action_and_modal(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    ingress: &InteractiveIngress,
    action: ActionId,
    busy: bool,
) -> (bool, Option<Line>) {
    let (handled, completed) = execute_action(status, ingress, action, busy);
    if completed {
        let effect = {
            let effects = status
                .lock()
                .expect("surface status lock")
                .surface_effect
                .clone();
            effects.lock().expect("surface action effect lock").take()
        };
        if let Some(line) =
            effect.and_then(|effect| apply_surface_effect(editor, phase, status, effect, busy))
        {
            return (true, Some(line));
        }
    }
    if handled && !completed {
        return (true, None);
    }
    if !matches!(action, ActionId::Model | ActionId::Thinking) {
        return (handled, None);
    }
    let picker = {
        let status = status.lock().expect("surface status lock");
        status.model_picker.clone()
    };
    let Some(picker) = picker else {
        return (handled, None);
    };
    if !handled {
        let diagnostic = picker
            .lock()
            .expect("model picker lock")
            .diagnostic()
            .map(ToString::to_string);
        if let Some(diagnostic) = diagnostic {
            status.lock().expect("surface status lock").notice = Some(diagnostic);
            repaint_status(status);
        }
        return (false, None);
    }
    if !picker.lock().expect("model picker lock").is_open() {
        return (handled, None);
    }
    (
        handled,
        run_model_picker(editor, phase, status, picker, busy, None),
    )
}

pub(super) fn apply_surface_effect(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    effect: SurfaceActionEffect,
    busy: bool,
) -> Option<Line> {
    match effect {
        SurfaceActionEffect::Mode => {
            return choose_policy_mode(editor, phase, status, busy);
        }
        SurfaceActionEffect::Plan => {
            let mut controller = PolicyControlController::new();
            return controller
                .select_mode(Mode::Plan)
                .map(|request| Line::HostCommand(HostCommand::Policy(request)));
        }
        SurfaceActionEffect::Yolo => {
            let mut controller = PolicyControlController::new();
            return controller
                .select_mode(Mode::Yolo)
                .map(|request| Line::HostCommand(HostCommand::Policy(request)));
        }
        SurfaceActionEffect::Permissions => {
            return choose_permission_action(editor, phase, status, busy);
        }
        SurfaceActionEffect::Refresh => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Refresh)));
        }
        SurfaceActionEffect::Compact => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Compact)));
        }
        SurfaceActionEffect::Fork => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Fork)));
        }
        SurfaceActionEffect::Sessions => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Sessions)));
        }
        SurfaceActionEffect::Usage => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Usage)));
        }
        SurfaceActionEffect::New => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::New)));
        }
        SurfaceActionEffect::Export => {
            if busy {
                status.lock().expect("surface status lock").notice =
                    Some("export is unavailable while a turn is running".to_owned());
            } else {
                // The shortcut and the menu carry no words, so they select the
                // default the slash grammar documents: sanitized markdown.
                return Some(Line::HostCommand(HostCommand::Export(
                    crate::repl::actions::ExportSelection::default(),
                )));
            }
        }
        SurfaceActionEffect::Name => {
            return route_session_metadata(
                status,
                crate::cli::SessionMetadataOperation::Name(crate::cli::NameOperation::Show),
                busy,
            );
        }
        SurfaceActionEffect::Goal => {
            return route_session_metadata(
                status,
                crate::cli::SessionMetadataOperation::Goal(crate::cli::GoalOperation::Show),
                busy,
            );
        }
        SurfaceActionEffect::Expand => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Expand)));
        }
        SurfaceActionEffect::Help => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Help)));
        }
        SurfaceActionEffect::Clear => {
            let buffer = editor.current_buffer_contents().to_string();
            if !buffer.trim().is_empty() && buffer.trim() != "/clear" {
                match confirm_surface_action(
                    editor,
                    phase,
                    status,
                    "Clear the unsent composer draft? [y/N] ",
                    busy,
                ) {
                    Confirmation::Confirmed => {}
                    Confirmation::Declined => {
                        status.lock().expect("surface status lock").notice =
                            Some("composer clear cancelled".to_string());
                        repaint_status(status);
                        return None;
                    }
                    Confirmation::Propagate(line) => return Some(line),
                }
            }
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").notice = None;
        }
        SurfaceActionEffect::Statusline => {
            let segments = match choose_statusline_preset(editor, phase, status, busy) {
                StatuslineChoice::Selected(segments) => segments,
                StatuslineChoice::Cancelled => return None,
                StatuslineChoice::Propagate(line) => return Some(line),
            };
            configure_statusline(status, segments);
            return None;
        }
        SurfaceActionEffect::Mcp if !busy => {
            return Some(Line::HostCommand(HostCommand::Mcp(String::new())));
        }
        SurfaceActionEffect::Mcp => {
            let actions = status
                .lock()
                .expect("surface status lock")
                .mcp_actions
                .clone()
                .expect("the enabled MCP action has an installed handler");
            let registry_status = actions.registry.status();
            let notice = if registry_status.servers().is_empty() {
                "mcp: no admitted servers".to_string()
            } else {
                format!("mcp: {:?}", registry_status.servers())
            };
            status.lock().expect("surface status lock").notice = Some(notice);
        }
        SurfaceActionEffect::Tasks => {
            return Some(Line::HostCommand(HostCommand::Tasks(String::new())));
        }
        SurfaceActionEffect::Tools => {
            let actions = status
                .lock()
                .expect("surface status lock")
                .mcp_actions
                .clone()
                .expect("the enabled tools action has an installed handler");
            let notice = match actions.registry.capture() {
                Ok(captured) => {
                    let native = actions
                        .native_tools
                        .iter()
                        .map(|tool| format!("native/{}", tool.descriptor().name));
                    let foreign = captured.tools().iter().map(|tool| {
                        format!("mcp:{}/{}", captured.generation(), tool.descriptor().name)
                    });
                    let catalogue = native.chain(foreign).collect::<Vec<_>>();
                    if catalogue.is_empty() {
                        "tools: none".to_string()
                    } else {
                        format!("tools: {}", catalogue.join(" "))
                    }
                }
                Err(_) => "tools: the admitted tool catalogue is unavailable".to_string(),
            };
            status.lock().expect("surface status lock").notice = Some(notice);
        }
        SurfaceActionEffect::Skills => {
            if busy {
                status.lock().expect("surface status lock").notice =
                    Some("skill actions are unavailable while a turn is running".to_owned());
            } else {
                return Some(Line::HostCommand(HostCommand::Skills(
                    SkillSurfaceAction::List,
                )));
            }
        }
        SurfaceActionEffect::Hooks => {
            if busy {
                status.lock().expect("surface status lock").notice =
                    Some("hook inspection is unavailable while a turn is running".to_owned());
            } else {
                return Some(Line::HostCommand(HostCommand::Hooks));
            }
        }
        SurfaceActionEffect::Config => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Config)));
        }
        SurfaceActionEffect::Setup => {
            if !busy {
                return Some(Line::HostCommand(HostCommand::Setup(String::new())));
            }
        }
        SurfaceActionEffect::Doctor => {
            // The whole snapshot, not the MCP corner of one. `/doctor` and
            // `ego doctor` render the same value now, so an operator cannot be
            // told two different things about the same installation depending
            // on which surface they happened to ask from.
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Doctor)));
        }
        SurfaceActionEffect::Quit => {
            let (drafts, queue) = {
                let current = status.lock().expect("surface status lock");
                (
                    current.drafts,
                    current.queue.pending_steers + current.queue.queued,
                )
            };
            let composer = editor.current_buffer_contents().trim();
            let composer_pending = !composer.is_empty() && composer != "/quit";
            if composer_pending || drafts > 0 || queue > 0 {
                let prompt = format!(
                    "Exit with {} composer draft, {drafts} saved drafts, and {queue} accepted prompts pending? [y/N] ",
                    usize::from(composer_pending)
                );
                match confirm_surface_action(editor, phase, status, &prompt, busy) {
                    Confirmation::Confirmed => {}
                    Confirmation::Declined => {
                        status.lock().expect("surface status lock").notice =
                            Some("quit cancelled; all unsent input was retained".to_string());
                        repaint_status(status);
                        return None;
                    }
                    Confirmation::Propagate(line) => return Some(line),
                }
            }
            return Some(Line::Ended);
        }
        SurfaceActionEffect::Version => {
            status.lock().expect("surface status lock").notice = Some(crate::build_info::summary());
        }
    }
    repaint_status(status);
    None
}

pub(super) fn route_session_metadata(
    status: &SharedStatus,
    operation: crate::cli::SessionMetadataOperation,
    busy: bool,
) -> Option<Line> {
    if busy {
        status.lock().expect("surface status lock").notice =
            Some("session metadata is unavailable while a turn is running".to_owned());
        repaint_status(status);
        None
    } else {
        Some(Line::HostCommand(HostCommand::SessionMetadata(operation)))
    }
}
