use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PermissionMenuId {
    List,
    Allow,
    Deny,
    Revoke,
    Remember,
    Explain,
}

impl PermissionMenuId {
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Revoke => "revoke",
            Self::Remember => "remember",
            Self::Explain => "explain",
        }
    }
}

pub(super) fn permission_menu() -> [PermissionMenuId; 6] {
    [
        PermissionMenuId::List,
        PermissionMenuId::Allow,
        PermissionMenuId::Deny,
        PermissionMenuId::Revoke,
        PermissionMenuId::Remember,
        PermissionMenuId::Explain,
    ]
}

enum PermissionField {
    Value(String),
    Cancelled,
    Propagate(Line),
}

enum PermissionCallField {
    Value {
        call: ego_core::permit::PreparedCall,
        workspace: CanonPath,
    },
    Cancelled(String),
    Propagate(Line),
}

pub(super) fn choose_permission_action(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
) -> Option<Line> {
    let choices = permission_menu();
    let options = choices
        .iter()
        .map(|choice| (choice.as_str(), choice.as_str()))
        .collect::<Vec<_>>();
    let choice = match select_permission_value(
        editor,
        phase,
        status,
        busy,
        "Choose permission operation",
        &options,
        (true, false, None),
    ) {
        PermissionField::Value(id) => choices
            .iter()
            .copied()
            .find(|choice| choice.as_str() == id)?,
        PermissionField::Cancelled => return None,
        PermissionField::Propagate(line) => return Some(line),
    };
    build_permission_request(editor, phase, status, busy, choice)
}

fn build_permission_request(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
    choice: PermissionMenuId,
) -> Option<Line> {
    macro_rules! field {
        ($prompt:literal) => {
            match read_permission_field(editor, phase, status, busy, $prompt) {
                PermissionField::Value(value) => value,
                PermissionField::Cancelled => return None,
                PermissionField::Propagate(line) => return Some(line),
            }
        };
    }
    let request = match choice {
        PermissionMenuId::List => PolicyControlRequest::Permission(PermissionInput::List),
        PermissionMenuId::Allow | PermissionMenuId::Deny => {
            let raw = field!("Canonical permission rule: ");
            let rule = match managed_rule(&raw) {
                Ok(rule) => rule,
                Err(error) => return permission_failure(status, error),
            };
            PolicyControlRequest::Rule(RuleMutation {
                effect: if choice == PermissionMenuId::Allow {
                    ManagedRuleEffect::Allow
                } else {
                    ManagedRuleEffect::Deny
                },
                rule,
            })
        }
        PermissionMenuId::Revoke => {
            let raw = field!("Canonical permission rule ID: ");
            let id = match managed_rule_id(raw) {
                Ok(id) => id,
                Err(error) => return permission_failure(status, error),
            };
            PolicyControlRequest::Permission(PermissionInput::Revoke(id))
        }
        PermissionMenuId::Remember => {
            let raw_effect = match select_permission_value(
                editor,
                phase,
                status,
                busy,
                "Choose rule effect",
                &[("allow", "Allow"), ("deny", "Deny")],
                (false, false, None),
            ) {
                PermissionField::Value(value) => value,
                PermissionField::Cancelled => return None,
                PermissionField::Propagate(line) => return Some(line),
            };
            let effect = match managed_rule_effect(&raw_effect) {
                Ok(effect) => effect,
                Err(error) => return permission_failure(status, error),
            };
            let (call, workspace) = match read_permission_call(editor, phase, status, busy) {
                PermissionCallField::Value { call, workspace } => (call, workspace),
                PermissionCallField::Cancelled(error) => {
                    return permission_failure(status, error);
                }
                PermissionCallField::Propagate(line) => return Some(line),
            };
            PolicyControlRequest::Permission(PermissionInput::Remember(RememberRequest::new(
                call, effect, workspace,
            )))
        }
        PermissionMenuId::Explain => {
            let (call, workspace) = match read_permission_call(editor, phase, status, busy) {
                PermissionCallField::Value { call, workspace } => (call, workspace),
                PermissionCallField::Cancelled(error) => {
                    return permission_failure(status, error);
                }
                PermissionCallField::Propagate(line) => return Some(line),
            };
            PolicyControlRequest::Permission(PermissionInput::Explain { call, workspace })
        }
    };
    Some(Line::HostCommand(HostCommand::Policy(request)))
}

fn read_permission_field(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
    text: &str,
) -> PermissionField {
    let saved = enter_modal(editor, phase, status);
    let prompt = Question::new(text.to_owned(), Arc::clone(status));
    let field = match answer(false, editor.read_line(&prompt)) {
        Line::Submitted(value) => PermissionField::Value(value),
        line @ (Line::Ended | Line::ExternalBreak(_)) => PermissionField::Propagate(line),
        Line::Blank | Line::Interrupted | Line::Refused(_) | Line::HostCommand(_) => {
            PermissionField::Cancelled
        }
    };
    leave_modal(
        editor,
        phase,
        status,
        saved,
        if busy {
            EditPhase::Busy
        } else {
            EditPhase::Idle
        },
    );
    field
}

fn permission_tools(status: &SharedStatus) -> Result<(Vec<Arc<dyn Tool>>, CanonPath), String> {
    let (actions, cwd) = {
        let current = status.lock().map_err(|_| "surface status is unavailable")?;
        let actions = current
            .mcp_actions
            .clone()
            .ok_or("the permission tool catalogue is unavailable")?;
        let cwd = current
            .status_base
            .as_ref()
            .map(|base| base.cwd.clone())
            .ok_or("the permission workspace is unavailable")?;
        (actions, cwd)
    };
    let workspace = OsFs
        .canonicalize(&cwd)
        .map_err(|error| format!("permission workspace could not be resolved: {error}"))?;
    let capture = actions
        .registry
        .capture()
        .map_err(|_| "the admitted permission tool catalogue is unavailable".to_owned())?;
    let mut admitted = actions
        .native_tools
        .iter()
        .filter_map(RegisteredTool::as_effect)
        .cloned()
        .collect::<Vec<Arc<dyn Tool>>>();
    admitted.extend(capture.tools().iter().cloned());
    Ok((admitted, workspace))
}

fn read_permission_call(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
) -> PermissionCallField {
    let (tools, workspace) = match permission_tools(status) {
        Ok(tools) => tools,
        Err(error) => return PermissionCallField::Cancelled(error),
    };
    let choices = tools
        .iter()
        .map(|tool| {
            let descriptor = tool.descriptor();
            (descriptor.name.clone(), descriptor.description.clone())
        })
        .collect::<Vec<_>>();
    let borrowed = choices
        .iter()
        .map(|(id, label)| (id.as_str(), label.as_str()))
        .collect::<Vec<_>>();
    let name = match select_permission_value(
        editor,
        phase,
        status,
        busy,
        "Choose admitted tool",
        &borrowed,
        (false, false, None),
    ) {
        PermissionField::Value(value) => value,
        PermissionField::Cancelled => {
            return PermissionCallField::Cancelled("tool selection was cancelled".into());
        }
        PermissionField::Propagate(line) => return PermissionCallField::Propagate(line),
    };
    let tool = tools
        .iter()
        .find(|tool| tool.descriptor().name == name)
        .cloned();
    let Some(tool) = tool else {
        return PermissionCallField::Cancelled("the selected tool is no longer admitted".into());
    };
    let schema = match serde_json::to_value(tool.descriptor().schema) {
        Ok(schema) => schema,
        Err(error) => {
            return PermissionCallField::Cancelled(format!(
                "tool schema could not be rendered: {error}"
            ));
        }
    };
    let arguments = match edit_permission_parameters(
        editor,
        phase,
        status,
        busy,
        schema,
        serde_json::json!({}),
    ) {
        PermissionEditOutcome::Value(arguments) => arguments,
        PermissionEditOutcome::Cancelled => {
            return PermissionCallField::Cancelled("tool parameter editing was cancelled".into());
        }
        PermissionEditOutcome::Propagate(line) => return PermissionCallField::Propagate(line),
    };
    let call = match prepare_from_catalogue(&name, &arguments, &workspace, &OsFs, &tools) {
        Ok(call) => call,
        Err(error) => return PermissionCallField::Cancelled(error.to_string()),
    };
    PermissionCallField::Value { call, workspace }
}

fn select_permission_value(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
    prompt: &str,
    choices: &[(&str, &str)],
    (cancel_missing_key, ignore_unexpected_line, initial_choice): (bool, bool, Option<&str>),
) -> PermissionField {
    if choices.is_empty() {
        return PermissionField::Cancelled;
    }
    let request = QuestionRequest::new(
        prompt,
        QuestionMode::Single,
        choices
            .iter()
            .map(|(id, label)| QuestionOption::new(*id, *label))
            .collect(),
        false,
    );
    let selector = Arc::new(Mutex::new(Selector::with_options(
        SelectorKind::Single,
        request
            .options
            .iter()
            .map(|option| SelectorOption::new(option.id.clone(), option.label.clone())),
    )));
    if let Some(index) =
        initial_choice.and_then(|id| request.options.iter().position(|option| option.id == id))
    {
        selector
            .lock()
            .expect("permission value selector lock")
            .key(SelectorKey::Number(index + 1));
    }
    let saved = enter_modal(editor, phase, status);
    status
        .lock()
        .expect("surface status lock")
        .question_selector = Some(Arc::clone(&selector));
    repaint_status(status);
    let result = loop {
        let question = Question::new(
            question_selector_prompt(&request, &selector, status),
            Arc::clone(status),
        );
        match queued_selector_line(status).unwrap_or_else(|| {
            let line = answer(false, editor.read_line(&question));
            let _ = editor.erase_last_prompt();
            line
        }) {
            Line::HostCommand(HostCommand::QuestionSelector(command)) => {
                let Some(key) = next_selector_key(status, &command) else {
                    if cancel_missing_key {
                        break PermissionField::Cancelled;
                    }
                    continue;
                };
                let mut selector = selector.lock().expect("permission value selector lock");
                selector.key(key);
                if key == SelectorKey::Escape {
                    break PermissionField::Cancelled;
                }
                if key != SelectorKey::Enter {
                    drop(selector);
                    repaint_status(status);
                    continue;
                }
                let Some(SelectorValue::Single(id)) = selector.result() else {
                    continue;
                };
                break PermissionField::Value(id.clone());
            }
            line @ (Line::Ended | Line::ExternalBreak(_)) => {
                break PermissionField::Propagate(line);
            }
            Line::Blank
            | Line::Interrupted
            | Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                break PermissionField::Cancelled;
            }
            Line::Submitted(_) | Line::Refused(_) | Line::HostCommand(_) => {
                if !ignore_unexpected_line {
                    break PermissionField::Cancelled;
                }
            }
        }
    };
    status
        .lock()
        .expect("surface status lock")
        .question_selector = None;
    leave_modal(
        editor,
        phase,
        status,
        saved,
        if busy {
            EditPhase::Busy
        } else {
            EditPhase::Idle
        },
    );
    result
}

fn permission_failure(status: &SharedStatus, error: impl std::fmt::Display) -> Option<Line> {
    status.lock().expect("surface status lock").notice =
        Some(format!("permission control failed: {error}"));
    repaint_status(status);
    None
}

pub(super) fn choose_policy_mode(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
) -> Option<Line> {
    let current = {
        let status = status.lock().expect("surface status lock");
        status
            .selected_mode
            .or_else(|| status.status_base.as_ref().map(|base| base.mode))
    };
    let Some(current) = current else {
        return permission_failure(status, "the effective execution mode is unavailable");
    };
    let mut controller = PolicyControlController::new();
    let modes = controller.mode_options();
    let options = modes
        .iter()
        .map(|mode| {
            let suffix = if *mode == current { " ✓ current" } else { "" };
            (
                mode.as_str(),
                format!(
                    "{}{suffix}",
                    if *mode == Mode::Yolo {
                        "yolo (bypass permissions)"
                    } else {
                        mode.as_str()
                    }
                ),
            )
        })
        .collect::<Vec<_>>();
    let borrowed = options
        .iter()
        .map(|(id, label)| (*id, label.as_str()))
        .collect::<Vec<_>>();
    match select_permission_value(
        editor,
        phase,
        status,
        busy,
        "Choose permission mode",
        &borrowed,
        (true, false, Some(current.as_str())),
    ) {
        PermissionField::Value(id) => modes
            .iter()
            .copied()
            .find(|mode| mode.as_str() == id)
            .and_then(|mode| controller.select_mode(mode))
            .map(|request| Line::HostCommand(HostCommand::Policy(request))),
        PermissionField::Cancelled | PermissionField::Propagate(Line::Ended) => None,
        PermissionField::Propagate(line) => Some(line),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Confirmation {
    Confirmed,
    Declined,
    Propagate(Line),
}

pub(super) fn confirm_surface_action(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    text: &str,
    busy: bool,
) -> Confirmation {
    let saved = enter_modal(editor, phase, status);
    let prompt = Question::new(text.to_string(), Arc::clone(status));
    let answer = loop {
        match answer(false, editor.read_line(&prompt)) {
            Line::Submitted(answer) => match parse_trust(&answer) {
                Some(Trust::Trusted) => break Confirmation::Confirmed,
                Some(Trust::Untrusted) => break Confirmation::Declined,
                None => continue,
            },
            Line::HostCommand(HostCommand::ModalEscape) => {
                break Confirmation::Declined;
            }
            Line::Interrupted | Line::Blank => break Confirmation::Declined,
            line @ (Line::Ended | Line::ExternalBreak(_)) => {
                break Confirmation::Propagate(line);
            }
            Line::Refused(_) | Line::HostCommand(_) => {}
        }
    };
    leave_modal(
        editor,
        phase,
        status,
        saved,
        if busy {
            EditPhase::Busy
        } else {
            EditPhase::Idle
        },
    );
    answer
}

pub(super) enum StatuslineChoice {
    Selected(String),
    Cancelled,
    Propagate(Line),
}

/// Each preset stores a complete template, not a second configuration grammar.
pub(super) const STATUSLINE_PRESETS: [(&str, &str, &str); 4] = [
    ("off", "Off", "{{ prompt }}"),
    (
        "minimal",
        "Minimal — model, effort, mode",
        "{{ model }} · {{ effort }} · {{ mode }}\n{{ prompt }}",
    ),
    (
        "default",
        "Default — model, effort, fast, mode, context, cost, queue, drafts, interrupt, cwd, git, trust",
        super::super::status::DEFAULT_TEMPLATE,
    ),
    (
        "full",
        "Full — model, effort, mode, context, cost, queue, drafts, interrupt, cwd, git, trust",
        super::super::status::DEFAULT_TEMPLATE,
    ),
];

pub(super) fn choose_statusline_preset(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
) -> StatuslineChoice {
    let choices = STATUSLINE_PRESETS
        .iter()
        .map(|(id, label, _)| (*id, *label))
        .collect::<Vec<_>>();
    match select_permission_value(
        editor,
        phase,
        status,
        busy,
        "Choose status-line setup\n▰/▱: context used/window; green <60%, yellow ≥60%, red ≥85%.\n≈$: subscription-equivalent (standard API), not billed; $: billed.\n(partial): mixed/unpriced usage; ?: context unmeasured.\n⎇: branch; ●: uncommitted changes; path: abbreviated cwd.",
        &choices,
        (false, true, None),
    ) {
        PermissionField::Value(id) => STATUSLINE_PRESETS
            .iter()
            .find(|(preset, _, _)| *preset == id)
            .map(|(_, _, template)| StatuslineChoice::Selected((*template).to_owned()))
            .unwrap_or(StatuslineChoice::Cancelled),
        PermissionField::Cancelled => StatuslineChoice::Cancelled,
        PermissionField::Propagate(line) => StatuslineChoice::Propagate(line),
    }
}

pub(super) fn configure_statusline(status: &SharedStatus, template: String) {
    let store = status
        .lock()
        .expect("surface status lock")
        .config_store
        .clone();
    let Some(store) = store else {
        status.lock().expect("surface status lock").notice =
            Some("the user-config mutation controller is not installed".to_string());
        repaint_status(status);
        return;
    };
    match store.mutate(UserConfigMutation::Statusline {
        template: template.clone(),
    }) {
        Ok(result) => {
            let mut current = status.lock().expect("surface status lock");
            let enabled = template.trim() != "{{ prompt }}";
            current.template = super::super::status::StatusTemplate::new(template);
            current.template_diagnostic = None;
            current.notice = Some(match result.publication {
                UserConfigPublication::NotPublished if enabled => {
                    "status line already enabled; configuration unchanged".to_string()
                }
                UserConfigPublication::NotPublished => {
                    "status line already disabled; configuration unchanged".to_string()
                }
                UserConfigPublication::PublishedDurably if enabled => {
                    "status line enabled and saved".to_string()
                }
                UserConfigPublication::PublishedDurably => {
                    "status line disabled and saved".to_string()
                }
                UserConfigPublication::PublishedDurabilityUncertain { diagnostic } => {
                    format!("status line saved with uncertain durability: {diagnostic}")
                }
            });
        }
        Err(error) => {
            status.lock().expect("surface status lock").notice = Some(error.to_string());
        }
    }
    repaint_status(status);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: initializing the current option prevents navigation to a different mode.
    #[test]
    fn initialized_permission_selector_still_applies_navigation() {
        let mut editor = Reedline::create();
        let phase = SurfacePhase::new(EditPhase::Idle);
        let status = Arc::new(Mutex::new(SurfaceStatus {
            picker_keys: vec!["down".into(), "enter".into()],
            ..SurfaceStatus::default()
        }));
        let result = select_permission_value(
            &mut editor,
            &phase,
            &status,
            false,
            "Choose permission mode",
            &[("plan", "Plan"), ("default", "Default"), ("auto", "Auto")],
            (true, false, Some("default")),
        );
        assert!(matches!(result, PermissionField::Value(id) if id == "auto"));
        assert!(status.lock().unwrap().question_selector.is_none());
    }

    // Catches: a preselected option is accepted on Escape instead of cancelling.
    #[test]
    fn initialized_permission_selector_cancels_without_accepting() {
        let mut editor = Reedline::create();
        editor.run_edit_commands(&[EditCommand::InsertString("draft text".into())]);
        let phase = SurfacePhase::new(EditPhase::Idle);
        let status = Arc::new(Mutex::new(SurfaceStatus {
            picker_keys: vec!["escape".into()],
            ..SurfaceStatus::default()
        }));
        let result = select_permission_value(
            &mut editor,
            &phase,
            &status,
            false,
            "Choose permission mode",
            &[("plan", "Plan"), ("default", "Default")],
            (true, false, Some("default")),
        );
        assert!(matches!(result, PermissionField::Cancelled));
        assert_eq!(editor.current_buffer_contents(), "draft text");
        assert_eq!(phase.get(), EditPhase::Idle);
        assert!(status.lock().unwrap().question_selector.is_none());
    }

    // Catches: sharing the selector silently retries a missing action-menu key.
    #[test]
    fn action_menu_cancels_when_next_selector_key_returns_none() {
        let mut editor = Reedline::create();
        editor.run_edit_commands(&[
            EditCommand::InsertString("draft text".into()),
            EditCommand::MoveToPosition {
                position: 5,
                select: false,
            },
        ]);
        let phase = SurfacePhase::new(EditPhase::Busy);
        let status = Arc::new(Mutex::new(SurfaceStatus {
            origin: InputOrigin::BracketedPaste,
            picker_keys: vec!["invalid-key".into(), "enter".into()],
            ..SurfaceStatus::default()
        }));
        assert!(choose_permission_action(&mut editor, &phase, &status, true).is_none());
        assert_eq!(editor.current_buffer_contents(), "draft text");
        assert_eq!(editor.current_insertion_point(), 5);
        assert_eq!(phase.get(), EditPhase::Busy);
        let status = status.lock().unwrap();
        assert_eq!(status.origin, InputOrigin::BracketedPaste);
        assert!(status.question_selector.is_none());
        assert!(status.picker_keys.is_empty());
    }
}
