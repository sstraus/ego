use super::*;

fn choose_mcp_control(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
) -> Option<Line> {
    use super::permission::{PermissionField, read_permission_field, select_permission_value};
    let actions = status
        .lock()
        .expect("surface status lock")
        .mcp_actions
        .clone()?;
    let current = actions.registry.status();
    let names = actions
        .servers
        .iter()
        .map(|(name, disabled)| {
            let state = if *disabled {
                "off (session)"
            } else {
                current
                    .server(name)
                    .map(|server| crate::mcp::server_state_text(server.state()))
                    .unwrap_or("absent")
            };
            (format!("server:{name}"), format!("{name}  {state}"))
        })
        .collect::<Vec<_>>();
    let mut choices = vec![
        ("action:status", "Show server status"),
        ("action:add", "Add server"),
    ];
    let borrowed = names
        .iter()
        .map(|(id, label)| (id.as_str(), label.as_str()))
        .collect::<Vec<_>>();
    choices.extend(borrowed);
    let selected = match select_permission_value(
        editor,
        phase,
        status,
        false,
        "Choose MCP server or action",
        &choices,
        (true, false, None),
    ) {
        PermissionField::Value(value) => value,
        PermissionField::Cancelled => return None,
        PermissionField::Propagate(line) => return Some(line),
    };
    if selected == "action:status" {
        return Some(Line::HostCommand(HostCommand::Mcp(String::new())));
    }
    if selected == "action:add" {
        return match read_permission_field(
            editor,
            phase,
            status,
            false,
            "Add: NAME --http URL or NAME --stdio -- COMMAND ...: ",
        ) {
            PermissionField::Value(value) => {
                Some(Line::HostCommand(HostCommand::Mcp(format!("add {value}"))))
            }
            PermissionField::Cancelled => None,
            PermissionField::Propagate(line) => Some(line),
        };
    }
    let action = match select_permission_value(
        editor,
        phase,
        status,
        false,
        "Choose MCP server control",
        &[
            ("toggle", "Turn on/off for this session"),
            ("login", "Authorize and reconnect"),
        ],
        (true, false, None),
    ) {
        PermissionField::Value(value) => value,
        PermissionField::Cancelled => return None,
        PermissionField::Propagate(line) => return Some(line),
    };
    Some(Line::HostCommand(HostCommand::Mcp(format!(
        "{action} {}",
        selected.strip_prefix("server:")?
    ))))
}

fn choose_skill_control(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
) -> Option<Line> {
    use super::permission::{PermissionField, select_permission_value};
    let registry = status
        .lock()
        .expect("surface status lock")
        .actions
        .as_ref()?
        .registry
        .clone();
    let skills = registry.skill_menu("");
    let mut choices = vec![
        ("action:list", "Show skills"),
        ("action:details", "Show sources and details"),
        ("action:diagnose", "Inspect admission problems"),
        ("action:rescan", "Reload skill catalogue"),
    ];
    let named = skills
        .iter()
        .map(|(name, description)| (format!("skill:{name}"), format!("{name} — {description}")))
        .collect::<Vec<_>>();
    let borrowed = named
        .iter()
        .map(|(id, description)| (id.as_str(), description.as_str()))
        .collect::<Vec<_>>();
    choices.extend(borrowed);
    let selected = match select_permission_value(
        editor,
        phase,
        status,
        false,
        "Choose skill action or skill to invoke",
        &choices,
        (true, false, None),
    ) {
        PermissionField::Value(value) => value,
        PermissionField::Cancelled => return None,
        PermissionField::Propagate(line) => return Some(line),
    };
    let action = match selected.as_str() {
        "action:list" => SkillSurfaceAction::List,
        "action:details" => SkillSurfaceAction::Details,
        "action:diagnose" => SkillSurfaceAction::Diagnose,
        "action:rescan" => SkillSurfaceAction::Rescan,
        _ => {
            let name = selected.strip_prefix("skill:")?;
            let text = format!("/skills invoke {name}");
            return match registry.classify(registry.submit(text, InputOrigin::Typed)) {
                Dispatch::Skill(operation) => {
                    Some(Line::HostCommand(HostCommand::Skill(operation)))
                }
                _ => Some(Line::Refused("the selected skill is no longer available")),
            };
        }
    };
    Some(Line::HostCommand(HostCommand::Skills(action)))
}

pub(crate) fn render_action_help(registry: &ActionRegistry, width: usize) -> String {
    render_help(registry, width, false)
}

pub(crate) fn render_action_help_details(registry: &ActionRegistry, width: usize) -> String {
    render_help(registry, width, true)
}

fn render_help(registry: &ActionRegistry, width: usize, details: bool) -> String {
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
            &[
                ActionId::Model,
                ActionId::Thinking,
                ActionId::Fast,
                ActionId::Usage,
                ActionId::Context,
            ],
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
                ActionId::Pin,
                ActionId::Unpin,
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
                ActionId::Login,
                ActionId::Tasks,
                ActionId::Side,
                ActionId::Tools,
                ActionId::Skills,
                ActionId::Hooks,
                ActionId::Init,
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
        let availability = match registry.availability(action.id()) {
            Availability::Disabled { owner } => format!("unavailable: {owner}"),
            Availability::Busy | Availability::BusyWithReason { .. } => "busy".into(),
            Availability::Enabled => String::new(),
        };
        vec![
            format!("/{}", action.name()),
            action.description_text().into(),
            registry.shortcut_labels(action.id()).join(", "),
            availability,
        ]
    };
    let limit = width.max(1);
    let table = |rows: Vec<Vec<String>>| {
        if details {
            let mut rows = rows.into_iter().map(|row| {
                let row = if row.len() > 2 {
                    vec![row[0].clone(), format!("{} {} {}", row[1], row[2], row[3])]
                } else {
                    row
                };
                row.into_iter().map(crate::render::Line::plain).collect()
            });
            crate::render::table::Table {
                header: rows.next().unwrap_or_default(),
                rows: rows.collect(),
                align: Vec::new(),
            }
            .render(limit, crate::render::Depth::None)
            .iter()
            .map(crate::render::Line::text)
            .collect::<Vec<_>>()
        } else {
            crate::render::table::plain_table_with_width(rows, limit)
        }
    };
    let mut logical = vec!["εgo commands".to_owned()];
    for (section, ids) in SECTIONS {
        logical.push(String::new());
        logical.push(section.to_owned());
        let mut entries = vec![vec![
            "Command".into(),
            "Description".into(),
            "Key".into(),
            "State".into(),
        ]];
        entries.extend(
            registry
                .iter()
                .filter(|action| ids.contains(&action.id()))
                .map(&render),
        );
        logical.extend(table(entries));
    }
    let capabilities = registry
        .skill_menu("")
        .into_iter()
        .chain(registry.mcp_menu(""));
    let mut entries = vec![vec!["Command".into(), "Description".into()]];
    entries.extend(capabilities.map(|(name, description)| vec![format!("/{name}"), description]));
    if entries.len() > 1 {
        logical.push("Skills and custom commands".into());
        logical.extend(table(entries));
    }
    logical.push("Aliases: /resume [ID], /continue [ID], /history QUERY".to_owned());
    logical.push(
        "Unknown slash input is refused locally; add a leading space to send literal text."
            .to_owned(),
    );
    logical.push("Keys".to_owned());
    let mut keys = vec![vec!["Key".into(), "Action".into()]];
    keys.extend(
        registry
            .keys()
            .into_iter()
            .map(|key| vec![key.label, key.description.into()]),
    );
    logical.extend(table(keys));
    logical.push("/help details — full command syntax and key descriptions".into());
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
        .chain([
            "Shift+Tab mode".to_owned(),
            format!("/{}", registry.help(ActionId::Help).name()),
        ]);
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

/// First-prompt keys only; full and turn-time bindings remain in registry help.
pub(crate) fn render_startup_key_hints(registry: &ActionRegistry, width: usize) -> String {
    let width = if width == 0 { 80 } else { width.max(40) };
    let mut items = registry
        .keys()
        .into_iter()
        .find(|key| key.hint == "interrupts")
        .map(|key| vec![format!("{} interrupt", key.label)])
        .unwrap_or_default();
    if width >= 60 {
        items.push("Shift+Tab mode".into());
        if let Some(label) = registry.shortcut_labels(ActionId::Model).first() {
            items.push(format!("{label} model"));
        }
    }
    let help = format!("/{} keys", registry.help(ActionId::Help).name());
    while !items.is_empty()
        && crate::render::Line::plain(format!("  {} · {help}", items.join(" · "))).width() > width
    {
        items.pop();
    }
    items.push(help);
    format!("  {}", items.join(" · "))
}

fn resolve_model_query(status: &SharedStatus, query: &str) -> Result<HostCommand, String> {
    let picker = status
        .lock()
        .expect("surface status lock")
        .model_picker
        .clone()
        .ok_or_else(|| "model selection is unavailable".to_owned())?;
    let request = picker
        .lock()
        .expect("model picker lock")
        .resolve_fragment(query)
        .map_err(|error| error.to_string())?;
    Ok(HostCommand::ModelSwitch(request))
}

/// Both submission paths enforce the same busy-command boundary.
pub(super) fn busy_dispatch_refusal(dispatch: &Dispatch) -> Option<&'static str> {
    match dispatch {
        Dispatch::Details(
            ActionId::Hooks | ActionId::Tools | ActionId::Doctor | ActionId::Help,
        ) => None,
        Dispatch::Details(_) => Some("details require an idle session"),
        Dispatch::History(_) | Dispatch::Resume(_) => {
            Some("history is unavailable while a turn is running")
        }
        Dispatch::Skill(_) => Some("a skill cannot be invoked while a turn is running"),
        Dispatch::Skills(
            SkillSurfaceAction::List | SkillSurfaceAction::Details | SkillSurfaceAction::Diagnose,
        ) => None,
        Dispatch::Skills(_) => Some("skill catalogue replacement requires an idle session"),
        Dispatch::Fork(_) => Some("switching forks is unavailable while a turn is running"),
        Dispatch::McpContext(_) => {
            Some("MCP context commands are unavailable while a turn is running")
        }
        Dispatch::Mcp(arguments) if arguments.trim().is_empty() => None,
        Dispatch::Mcp(_) => Some("MCP registry and authentication changes require an idle session"),
        Dispatch::Login(None) => None,
        Dispatch::Login(Some(_)) => Some("login is unavailable while a turn is running"),
        Dispatch::Setup(_) => Some("setup is unavailable while a turn is running"),
        Dispatch::Init(_) => Some("init is unavailable while a turn is running"),
        Dispatch::Fast(_) => None,
        Dispatch::Hooks => None,
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

pub(super) fn refuse_busy_shell(status: &SharedStatus, text: &str) -> bool {
    if Line::bang_from_editor(text).is_none() {
        return false;
    }
    commit_to_transcript(
        status,
        "shell escapes require an idle terminal; wait for the current turn",
    );
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
    if refuse_busy_shell(status, &buffer) {
        return None;
    }
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
        Dispatch::Mode(mode) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((
                Line::HostCommand(HostCommand::Policy(PolicyControlRequest::SetMode(mode))),
                false,
            ));
        }
        Dispatch::ModelQuery(query) => match resolve_model_query(status, &query) {
            Ok(command) => {
                editor.run_edit_commands(&[EditCommand::Clear]);
                status.lock().expect("surface status lock").origin = InputOrigin::Typed;
                return Some((Line::HostCommand(command), false));
            }
            Err(message) => {
                status.lock().expect("surface status lock").notice = Some(message);
                repaint_status(status);
                return None;
            }
        },
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
            let line = route_session_metadata(status, operation, true)?;
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((line, false));
        }
        Dispatch::Config(arguments) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((Line::HostCommand(HostCommand::Config(arguments)), false));
        }
        Dispatch::Pins(command) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((Line::HostCommand(HostCommand::Pins(command)), false));
        }
        Dispatch::Tasks(arguments) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((Line::HostCommand(HostCommand::Tasks(arguments)), false));
        }
        Dispatch::Side(question) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((
                Line::HostCommand(HostCommand::SideQuestion(question)),
                false,
            ));
        }
        Dispatch::UnknownSlash(message) => {
            commit_to_transcript(status, &message);
            return None;
        }
        Dispatch::Refused { reason } => {
            commit_to_transcript(status, reason);
            return None;
        }
        Dispatch::Fast(enabled) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((Line::HostCommand(HostCommand::Fast(enabled)), false));
        }
        Dispatch::Details(action) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((Line::HostCommand(HostCommand::Details(action)), false));
        }
        Dispatch::Skills(action) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((Line::HostCommand(HostCommand::Skills(action)), false));
        }
        Dispatch::Export(selection) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((Line::HostCommand(HostCommand::Export(selection)), false));
        }
        Dispatch::Mcp(arguments) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((Line::HostCommand(HostCommand::Mcp(arguments)), false));
        }
        Dispatch::Hooks => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((Line::HostCommand(HostCommand::Hooks), false));
        }
        Dispatch::Login(None) => {
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            return Some((Line::HostCommand(HostCommand::Login(None)), false));
        }
        Dispatch::History(_)
        | Dispatch::Resume(_)
        | Dispatch::Skill(_)
        | Dispatch::Fork(_)
        | Dispatch::McpContext(_)
        | Dispatch::Login(Some(_))
        | Dispatch::Setup(_)
        | Dispatch::Init(_) => unreachable!("busy commands were refused before dispatch"),
        Dispatch::Prompt | Dispatch::Immediate | Dispatch::InteractiveIngress => {}
    }
    status
        .lock()
        .expect("surface status lock")
        .stage_queue_preview(buffer.clone(), 0);
    let result =
        super::ingress_wait::wait(editor, status, ingress.submit(buffer.clone(), Vec::new()));
    status
        .lock()
        .expect("surface status lock")
        .settle_queue_preview(result.is_ok());
    match result {
        Ok(queue) => {
            if let Err(error) = super::accept_queued_buffer(editor, &buffer) {
                super::report_terminal_failure("queued input", error);
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
            if steer && request_interrupt(editor, status, ingress) {
                return Some((Line::HostCommand(HostCommand::Interrupt), false));
            }
        }
        Err(ego_core::turn::ControlError::Closed) => {
            // Busy admission can close before the terminal observes idle.
            // Retire the composer with the normal next-turn receipt and history
            // before transferring input that has no accepted queue preview.
            if let Err(error) = editor.accept_buffer(prompt) {
                super::report_terminal_failure("output", error);
                return Some((Line::Ended, true));
            }
            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            restore_latest_draft(editor, status, drafts);
            return Some((
                Line::HostCommand(HostCommand::SubmittedDraft {
                    text: buffer,
                    images: Vec::new(),
                }),
                false,
            ));
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
        if phase.get() == EditPhase::Busy {
            let notice = status.lock().expect("surface status lock").notice.take();
            if let Some(notice) = notice {
                // Notices and other transcript output use the same Reedline
                // owner, which retires its live region before printing.
                commit_to_transcript(status, &format!("{}: {notice}", crate::DISPLAY_NAME));
            }
        } else if phase.get() == EditPhase::Idle {
            question::tell_host_current(status, |osc, _, current| {
                [
                    osc.state(current.native_activity),
                    osc.progress(crate::render::osc::Progress::Cleared),
                ]
                .concat()
            });
        }
        let signal = suspend::read_line(editor, prompt, status);
        // An attachment borrows only the terminal input seat. The parent's
        // coordinator keeps running, but no attached prompt may fall back to it
        // (including when the child's receiver has just closed).
        if phase.get() != EditPhase::Modal
            && let Some(child) = background::attached_input(status)
        {
            let text = match &signal {
                Ok(reedline::Signal::Success(text)) => Some((text.clone(), false)),
                Ok(reedline::Signal::HostCommand(command))
                    if command == BUSY_SUBMIT_COMMAND || command == BUSY_STEER_COMMAND =>
                {
                    Some((editor.current_buffer_contents().to_owned(), true))
                }
                _ => None,
            };
            if let Some((text, accept)) = text {
                let dispatch = {
                    let current = status.lock().expect("surface status lock");
                    current
                        .actions
                        .as_ref()
                        .map_or(Dispatch::Prompt, |actions| {
                            actions
                                .registry
                                .classify(actions.registry.submit(text.clone(), current.origin))
                        })
                };
                if let Dispatch::Tasks(arguments) = dispatch {
                    status.lock().expect("surface status lock").origin = InputOrigin::Typed;
                    if accept {
                        editor.run_edit_commands(&[EditCommand::Clear]);
                    }
                    return (Line::HostCommand(HostCommand::Tasks(arguments)), false);
                }
                if !matches!(dispatch, Dispatch::Prompt) || text.trim_start().starts_with('!') {
                    commit_to_transcript(
                        status,
                        "While attached, send text, use /tasks, or /detach; parent commands are unavailable.",
                    );
                    continue;
                }
                if text.trim().is_empty() {
                    continue;
                }
                match super::ingress_wait::wait(
                    editor,
                    status,
                    child.submit(text.clone(), Vec::new()),
                ) {
                    Ok(_) => {
                        status.lock().expect("surface status lock").origin = InputOrigin::Typed;
                        if accept && editor.accept_buffer(prompt).is_err() {
                            return (Line::Ended, true);
                        }
                        restore_latest_draft(editor, status, drafts);
                    }
                    Err(error) => {
                        // Attached input always submits with accept=true, so the
                        // typed buffer stays untouched and no parked draft is popped.
                        commit_to_transcript(
                            status,
                            &format!(
                                "Child input was not accepted: {error}. Use /detach to return."
                            ),
                        );
                    }
                }
                continue;
            }
            match &signal {
                Ok(reedline::Signal::CtrlC) => {
                    if let Err(error) = super::ingress_wait::wait(editor, status, child.interrupt())
                    {
                        commit_to_transcript(status, &error.to_string());
                    }
                    continue;
                }
                // Ctrl+S manages the attached session's own draft namespace.
                Ok(reedline::Signal::HostCommand(command)) if command == DRAFT_COMMAND => {}
                Ok(reedline::Signal::HostCommand(command)) => {
                    if command == BUSY_CANCEL_COMMAND || command == INTERRUPT_COMMAND {
                        if let Err(error) =
                            super::ingress_wait::wait(editor, status, child.interrupt())
                        {
                            commit_to_transcript(status, &error.to_string());
                        }
                    } else {
                        commit_to_transcript(
                            status,
                            "This parent control is unavailable while attached; use /detach first.",
                        );
                    }
                    continue;
                }
                // External wakes still service permission/question rendezvous.
                _ => {}
            }
        }

        if matches!(&signal, Ok(reedline::Signal::HostCommand(command)) if command == MODE_CYCLE_COMMAND)
        {
            // Preserve intent until the owner consumes this queued command. During
            // a turn, the editor's visible/staged mode can lag earlier commands.
            return (
                Line::HostCommand(HostCommand::Policy(PolicyControlRequest::CycleMode)),
                false,
            );
        }
        // Reject blank input before Reedline commits it to history or clears
        // the composer, for both idle submission and busy queue admission.
        let signal = if matches!(
            &signal,
            Ok(reedline::Signal::HostCommand(command))
                if command == BUSY_SUBMIT_COMMAND || command == BUSY_STEER_COMMAND
        ) {
            editor.accept_menu_selection();
            let buffer = editor.current_buffer_contents().to_string();
            if buffer.trim().is_empty()
                && status
                    .lock()
                    .expect("surface status lock")
                    .images
                    .is_empty()
            {
                let queued = {
                    let current = status.lock().expect("surface status lock");
                    current.queue.pending_steers + current.queue.queued > 0
                };
                if matches!(&signal, Ok(reedline::Signal::HostCommand(command)) if command == BUSY_STEER_COMMAND)
                    && phase.get() == EditPhase::Busy
                    && queued
                    && request_interrupt(editor, status, ingress)
                {
                    return (Line::HostCommand(HostCommand::Interrupt), false);
                }
                continue;
            }
            let is_prompt = {
                let current = status.lock().expect("surface status lock");
                current.actions.as_ref().is_none_or(|actions| {
                    matches!(
                        actions
                            .registry
                            .classify(actions.registry.submit(buffer.clone(), current.origin)),
                        Dispatch::Prompt
                    )
                })
            };
            if phase.get() == EditPhase::Idle && !is_prompt {
                editor
                    .accept_buffer(prompt)
                    .map(|()| reedline::Signal::Success(buffer))
            } else {
                signal
            }
        } else {
            signal
        };
        // Reedline's Ctrl+C cleared the draft before ending the read; the busy
        // cancel keeps the read suspended, so the host clears it instead.
        if matches!(&signal, Ok(reedline::Signal::HostCommand(command)) if command == BUSY_CANCEL_COMMAND)
        {
            editor.run_edit_commands(&[EditCommand::Clear]);
        }
        // Hold only the isolated PTY child at the real suspended-read boundary.
        #[cfg(test)]
        if asked_for_image.load(Ordering::Relaxed) && std::env::var_os("EGO_464_HOLD").is_some() {
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
        if let Line::Submitted(text) = &line {
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
            // The startup session browser precedes session action-sink setup.
            // Quit is a terminal lifecycle effect, so both browsers use its
            // existing draft/queue confirmation without a session action sink.
            if phase.get() == EditPhase::Modal
                && matches!(&dispatch, Dispatch::Action(ActionId::Quit))
                && status
                    .lock()
                    .expect("surface status lock")
                    .session_picker_open
            {
                if let Some(line) =
                    apply_surface_effect(editor, phase, status, SurfaceActionEffect::Quit, false)
                {
                    return (line, failed);
                }
                continue;
            }
            if phase.get() == EditPhase::Modal {
                // Unknown slash text is modal data, including absolute paths.
                // Registered commands never become answers. Explicit concurrent
                // inspections, goal/root controls, and future-session config
                // saves may cross without consuming or answering its decision.
                let command = match dispatch {
                    Dispatch::Prompt
                    | Dispatch::Immediate
                    | Dispatch::InteractiveIngress
                    | Dispatch::UnknownSlash(_) => return (line, failed),
                    Dispatch::Action(ActionId::Mcp) => Some(HostCommand::Mcp(String::new())),
                    Dispatch::Action(action) => Some(HostCommand::Action(action)),
                    Dispatch::Details(action) => Some(HostCommand::Details(action)),
                    Dispatch::Hooks => Some(HostCommand::Hooks),
                    Dispatch::Skills(action) => Some(HostCommand::Skills(action)),
                    Dispatch::Mcp(arguments) => Some(HostCommand::Mcp(arguments)),
                    Dispatch::Config(arguments) => Some(HostCommand::Config(arguments)),
                    Dispatch::Tasks(arguments) => Some(HostCommand::Tasks(arguments)),
                    Dispatch::Side(question) => Some(HostCommand::SideQuestion(question)),
                    Dispatch::Pins(command) => Some(HostCommand::Pins(command)),
                    Dispatch::SessionMetadata(operation) => {
                        Some(HostCommand::SessionMetadata(operation))
                    }
                    Dispatch::Refused { reason } => {
                        commit_to_transcript(status, reason);
                        continue;
                    }
                    _ => None,
                };
                let inspections = status
                    .lock()
                    .expect("surface status lock")
                    .inspections
                    .clone();
                if let Some(command) = command.filter(|command| {
                    command.is_concurrent_native()
                        && !matches!(command, HostCommand::Tasks(arguments) if !arguments.trim().is_empty())
                }) && let Some((sender, waker)) = inspections
                {
                    if sender.send(command).is_err() {
                        return (Line::Ended, failed);
                    }
                    waker.wake();
                } else {
                    commit_to_transcript(
                        status,
                        "this command is unavailable while a question or permission prompt is open; answer or cancel it first",
                    );
                }
                continue;
            }
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
                Dispatch::Details(action) => {
                    return (Line::HostCommand(HostCommand::Details(action)), failed);
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
                Dispatch::Skills(SkillSurfaceAction::List)
                    if text.trim() == "/skills" && phase.get() != EditPhase::Busy =>
                {
                    if let Some(line) = choose_skill_control(editor, phase, status) {
                        return (line, failed);
                    }
                    continue;
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
                Dispatch::Pins(command) => {
                    return (Line::HostCommand(HostCommand::Pins(command)), failed);
                }
                Dispatch::Tasks(arguments) => {
                    return (Line::HostCommand(HostCommand::Tasks(arguments)), failed);
                }
                Dispatch::Side(question) => {
                    return (
                        Line::HostCommand(HostCommand::SideQuestion(question)),
                        failed,
                    );
                }
                Dispatch::McpContext(command) => {
                    return (Line::HostCommand(HostCommand::McpContext(command)), failed);
                }
                Dispatch::Mcp(arguments)
                    if arguments.is_empty() && phase.get() != EditPhase::Busy =>
                {
                    if let Some(line) = choose_mcp_control(editor, phase, status) {
                        return (line, failed);
                    }
                    continue;
                }
                Dispatch::Mcp(arguments) => {
                    return (Line::HostCommand(HostCommand::Mcp(arguments)), failed);
                }
                Dispatch::Login(provider) => {
                    return (Line::HostCommand(HostCommand::Login(provider)), failed);
                }
                Dispatch::Setup(arguments) => {
                    return (Line::HostCommand(HostCommand::Setup(arguments)), failed);
                }
                Dispatch::Init(arguments) => {
                    return (Line::HostCommand(HostCommand::Init(arguments)), failed);
                }
                Dispatch::Config(arguments) => {
                    return (Line::HostCommand(HostCommand::Config(arguments)), failed);
                }
                Dispatch::Mode(mode) => {
                    return (
                        Line::HostCommand(HostCommand::Policy(PolicyControlRequest::SetMode(mode))),
                        failed,
                    );
                }
                Dispatch::Fast(enabled) => {
                    return (Line::HostCommand(HostCommand::Fast(enabled)), failed);
                }
                Dispatch::ModelQuery(query) => match resolve_model_query(status, &query) {
                    Ok(command) => return (Line::HostCommand(command), failed),
                    Err(message) => {
                        commit_to_transcript(status, &message);
                        continue;
                    }
                },
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
                    // Native submission is admitted before this draft is cleared
                    // or a different stashed draft is restored.
                    editor.run_edit_commands(&[
                        EditCommand::Clear,
                        EditCommand::InsertString(text.clone()),
                    ]);
                    return (
                        Line::HostCommand(HostCommand::AdmitPrompt {
                            text: text.clone(),
                            steer: false,
                        }),
                        failed,
                    );
                }
                Dispatch::Immediate | Dispatch::InteractiveIngress => return (line, failed),
            }
        }
        let Line::HostCommand(command) = line else {
            if matches!(line, Line::Interrupted) && phase.get() == EditPhase::Busy {
                match super::ingress_wait::wait(editor, status, ingress.interrupt()) {
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
            // A selector can consume a whole queued key burst before Reedline
            // delivers its remaining wakes. They belong to the closed modal,
            // not to the driver's ordinary command queue.
            HostCommand::QuestionSelector(_) if phase.get() != EditPhase::Modal => continue,
            HostCommand::Background if phase.get() == EditPhase::Modal => {
                return (Line::HostCommand(HostCommand::Background), false);
            }
            HostCommand::Background => {
                let notice = if phase.get() == EditPhase::Busy {
                    match super::ingress_wait::wait(editor, status, ingress.background()) {
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
                if buffer.is_empty()
                    && status
                        .lock()
                        .expect("surface status lock")
                        .images
                        .is_empty()
                {
                    if restore_latest_draft(editor, status, drafts) {
                        continue;
                    }
                } else {
                    let mut current = status.lock().expect("surface status lock");
                    let origin = current.origin;
                    let images = std::mem::take(&mut current.images);
                    current.image_chips.clear();
                    drop(current);
                    drafts.push_active(ComposerDraft {
                        images,
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
                match super::ingress_wait::wait(editor, status, ingress.recall()) {
                    Ok(Some(ego_core::turn::RecalledPrompt {
                        prompt: recalled,
                        queue,
                        pending,
                    })) => {
                        // Do not discard an unsent draft, including an earlier
                        // recalled prompt, when Up withdraws another queue entry.
                        let buffer = editor.current_buffer_contents().to_owned();
                        let cursor = editor.current_insertion_point();
                        let mut current = status.lock().expect("surface status lock");
                        if !buffer.is_empty() || !current.images.is_empty() {
                            drafts.push_active(ComposerDraft {
                                text: buffer,
                                cursor,
                                origin: current.origin,
                                images: std::mem::take(&mut current.images),
                            });
                        }
                        current.drafts = drafts.active_count();
                        let draft_count = current.drafts;
                        if let Some(snapshot) = current.snapshot.as_mut() {
                            snapshot.set_drafts(draft_count);
                        }
                        drop(current);
                        editor.run_edit_commands(&[EditCommand::Clear]);
                        editor.run_edit_commands(&[EditCommand::InsertString(
                            recalled.text().to_owned(),
                        )]);
                        let mut current = status.lock().expect("surface status lock");
                        current.images = super::super::ImageDraft::recalled(recalled.images());
                        current.image_chips = current.images.labels();
                        current.queue = queue;
                        current.reconcile_queue_previews(pending.iter());
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
                if phase.get() == EditPhase::Idle
                    || !status
                        .lock()
                        .expect("surface status lock")
                        .images
                        .is_empty()
                {
                    let text = editor.current_buffer_contents().to_owned();
                    let dispatch = {
                        let current = status.lock().expect("surface status lock");
                        current
                            .actions
                            .as_ref()
                            .map_or(Dispatch::Prompt, |actions| {
                                actions
                                    .registry
                                    .classify(actions.registry.submit(text.clone(), current.origin))
                            })
                    };
                    if matches!(dispatch, Dispatch::Prompt) {
                        return (
                            Line::HostCommand(HostCommand::AdmitPrompt {
                                text,
                                steer: command == HostCommand::BusySteer,
                            }),
                            false,
                        );
                    }
                    // Local slash actions still use the normal classification path.
                    if phase.get() == EditPhase::Idle {
                        let _ = editor.accept_buffer(prompt);
                        return (Line::Submitted(text), false);
                    }
                }
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
                if request_interrupt(editor, status, ingress) {
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
pub(super) fn request_interrupt(
    editor: &mut Reedline,
    status: &SharedStatus,
    ingress: &InteractiveIngress,
) -> bool {
    match super::ingress_wait::wait(editor, status, ingress.interrupt()) {
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

pub(super) fn restore_latest_draft(
    editor: &mut Reedline,
    status: &SharedStatus,
    drafts: &mut DraftStash,
) -> bool {
    if !editor.current_buffer_contents().is_empty()
        || !status
            .lock()
            .expect("surface status lock")
            .images
            .is_empty()
    {
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
        current.images = saved.images;
        current.image_chips = current.images.labels();
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
    editor: &mut Reedline,
    status: &SharedStatus,
    ingress: &InteractiveIngress,
    action: ActionId,
    busy: bool,
) -> (bool, bool) {
    if action == ActionId::Retry {
        let notice = if busy {
            match super::ingress_wait::wait(editor, status, ingress.retry_now()) {
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
            match super::ingress_wait::wait(editor, status, ingress.submit(canonical, Vec::new())) {
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
    let (handled, completed) = execute_action(editor, status, ingress, action, busy);
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
        SurfaceActionEffect::Fast => {
            status.lock().expect("surface status lock").notice =
                Some("usage: /fast on|off".to_owned());
            repaint_status(status);
        }
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
        SurfaceActionEffect::Context => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Context)));
        }
        SurfaceActionEffect::New => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::New)));
        }
        SurfaceActionEffect::Export => {
            // The shortcut and the menu carry no words, so they select the
            // default the slash grammar documents: sanitized markdown. Busy
            // or idle, the command reads only the committed prefix.
            return Some(Line::HostCommand(HostCommand::Export(
                crate::repl::actions::ExportSelection::default(),
            )));
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
            // The driver can hold its output lock while finish waits for this
            // thread to drain the printer. Enqueue through the owner's pump,
            // just like help and permission receipts, never through diagnostics.
            let detail = status.lock().expect("surface status lock").details.latest();
            match detail {
                Some(detail) => commit_transcript_block(status, &format!("\n{detail}")),
                None => commit_to_transcript(
                    status,
                    &format!("{}: no tool call to expand yet", crate::DISPLAY_NAME),
                ),
            }
        }
        SurfaceActionEffect::Help if busy => {
            let help = {
                let current = status.lock().expect("surface status lock");
                render_action_help(
                    &current
                        .actions
                        .as_ref()
                        .expect("installed actions")
                        .registry,
                    current.width,
                )
            };
            commit_to_transcript(status, &help);
        }
        SurfaceActionEffect::Help => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Help)));
        }
        SurfaceActionEffect::Clear => {
            if busy {
                status.lock().expect("surface status lock").notice = Some(
                    "cannot clear the conversation while work is pending; wait for it to finish"
                        .to_owned(),
                );
            } else {
                editor.run_edit_commands(&[EditCommand::Clear]);
                let mut current = status.lock().expect("surface status lock");
                current.images.clear();
                current.image_chips.clear();
                drop(current);
                return Some(Line::HostCommand(HostCommand::Action(ActionId::Clear)));
            }
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
        SurfaceActionEffect::Login => {
            // The menu entry is the bare provider inventory, never a flow;
            // a running turn admits it through the concurrent inbox.
            return Some(Line::HostCommand(HostCommand::Login(None)));
        }
        SurfaceActionEffect::Mcp if !busy => {
            return choose_mcp_control(editor, phase, status);
        }
        SurfaceActionEffect::Mcp => {
            return Some(Line::HostCommand(HostCommand::Mcp(String::new())));
        }
        SurfaceActionEffect::Pin => {
            return Some(Line::HostCommand(HostCommand::Pins(
                ego_core::compact::PinCommand::List,
            )));
        }
        SurfaceActionEffect::Unpin => {
            return Some(Line::Refused(
                "usage: /unpin NUMBER (use /pin to list pins)",
            ));
        }
        SurfaceActionEffect::Tasks => {
            return Some(Line::HostCommand(HostCommand::Tasks(String::new())));
        }
        SurfaceActionEffect::Side => {
            return Some(Line::Refused(crate::repl::actions::SIDE_GRAMMAR));
        }
        SurfaceActionEffect::Tools => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Tools)));
        }
        SurfaceActionEffect::Skills if busy => {
            return Some(Line::HostCommand(HostCommand::Skills(
                SkillSurfaceAction::List,
            )));
        }
        SurfaceActionEffect::Skills => {
            return choose_skill_control(editor, phase, status);
        }
        SurfaceActionEffect::Hooks => {
            return Some(Line::HostCommand(HostCommand::Hooks));
        }
        SurfaceActionEffect::Init => {
            if !busy {
                return Some(Line::HostCommand(HostCommand::Init(String::new())));
            }
            refuse_busy_dispatch(status, &Dispatch::Init(String::new()));
        }
        SurfaceActionEffect::Config => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Config)));
        }
        SurfaceActionEffect::Setup => {
            if !busy {
                return Some(Line::HostCommand(HostCommand::Setup(String::new())));
            }
            refuse_busy_dispatch(status, &Dispatch::Setup(String::new()));
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
                    Confirmation::Propagate(line) => return Some(*line),
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
    if busy && !HostCommand::SessionMetadata(operation.clone()).is_concurrent_native() {
        status.lock().expect("surface status lock").notice = Some(
            "cannot change the session name or goal during a reply; wait for it to finish"
                .to_owned(),
        );
        repaint_status(status);
        None
    } else {
        Some(Line::HostCommand(HostCommand::SessionMetadata(operation)))
    }
}

#[cfg(test)]
mod integration_control_tests {
    use super::*;

    // Catches: the live Skills action bypasses the selector and only lists skills.
    #[test]
    fn skill_picker_routes_selected_control_and_preserves_composer() {
        let registry = Arc::new(ActionRegistry::final_surface());
        let status = Arc::new(Mutex::new(SurfaceStatus {
            picker_keys: vec!["down".into(), "enter".into()],
            ..SurfaceStatus::with_registry(registry)
        }));
        let mut editor = Reedline::create();
        editor.run_edit_commands(&[EditCommand::InsertString("draft".into())]);
        let phase = SurfacePhase::new(EditPhase::Idle);
        let selected = apply_surface_effect(
            &mut editor,
            &phase,
            &status,
            SurfaceActionEffect::Skills,
            false,
        );
        assert!(matches!(
            selected,
            Some(Line::HostCommand(HostCommand::Skills(
                SkillSurfaceAction::Details
            )))
        ));
        assert_eq!(editor.current_buffer_contents(), "draft");
        assert_eq!(phase.get(), EditPhase::Idle);
    }

    // Catches: a terminal writer failure resubmits an already-sent busy prompt.
    #[test]
    fn busy_submit_writer_failure_does_not_hand_prompt_to_next_turn() {
        use super::super::ingress_wait::TestOutput;
        use ego_core::event::domain::CancelCause;
        use ego_core::turn::interactive_control;

        let (printer, rows) = bounded(1);
        printer.send("pending output".to_owned()).unwrap();
        let cancel = CancelToken::new();
        let written = Arc::new(Mutex::new(TestOutput {
            fail_writes: true,
            ..TestOutput::default()
        }));
        let status = Arc::new(Mutex::new(SurfaceStatus {
            printer_output: Some(rows),
            printer_cancel: Some(cancel.clone()),
            printer_test_output: Some(Arc::clone(&written)),
            ..SurfaceStatus::default()
        }));
        let (ingress, control) = interactive_control();
        let owner_status = Arc::clone(&status);
        let (done, completed) = bounded(1);
        let owner = std::thread::spawn(move || {
            let mut editor = Reedline::create();
            editor.run_edit_commands(&[EditCommand::InsertString("send exactly once".into())]);
            let result = submit_busy_line(
                &mut editor,
                &Waiting::new(Arc::clone(&owner_status)),
                &SurfacePhase::new(EditPhase::Busy),
                &owner_status,
                &ingress,
                &mut DraftStash::new(SessionId::new()),
                false,
            );
            done.send((
                result.is_none(),
                editor.current_buffer_contents().to_owned(),
            ))
            .unwrap();
        });
        let result = completed.recv_timeout(std::time::Duration::from_secs(2));
        drop(control);
        owner.join().unwrap();
        assert_eq!(result, Ok((true, "send exactly once".to_owned())));
        assert_eq!(cancel.cause(), Some(CancelCause::Host));
        assert_eq!(written.lock().unwrap().errors, ["injected writer failure"]);
        assert!(
            status
                .lock()
                .unwrap()
                .notice
                .as_ref()
                .is_some_and(|notice| notice.contains("terminal output failed"))
        );
    }

    // Catches: canceling a skill selector destroys the draft or runs the default action.
    #[test]
    fn skill_picker_escape_does_not_dispatch_or_destroy_draft() {
        let registry = Arc::new(ActionRegistry::final_surface());
        let status = Arc::new(Mutex::new(SurfaceStatus {
            picker_keys: vec!["escape".into()],
            ..SurfaceStatus::with_registry(registry)
        }));
        let mut editor = Reedline::create();
        editor.run_edit_commands(&[EditCommand::InsertString("draft".into())]);
        let phase = SurfacePhase::new(EditPhase::Idle);
        assert!(
            apply_surface_effect(
                &mut editor,
                &phase,
                &status,
                SurfaceActionEffect::Skills,
                false,
            )
            .is_none()
        );
        assert_eq!(editor.current_buffer_contents(), "draft");
        assert_eq!(phase.get(), EditPhase::Idle);
    }
}
