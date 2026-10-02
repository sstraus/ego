use super::*;

enum PolicyModeChoice {
    Selected(Mode),
    Cancelled,
    Propagate(Line),
}

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

enum PermissionMenuChoice {
    Selected(PermissionMenuId),
    Cancelled,
    Propagate(Line),
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
    let request = QuestionRequest::new(
        "Choose permission operation",
        QuestionMode::Single,
        choices
            .iter()
            .map(|choice| QuestionOption::new(choice.as_str(), choice.as_str()))
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
    let saved = enter_modal(editor, phase, status);
    status
        .lock()
        .expect("surface status lock")
        .question_selector = Some(Arc::clone(&selector));
    repaint_status(status);

    let choice = loop {
        let prompt = Question::new(
            question_selector_prompt(&request, &selector),
            Arc::clone(status),
        );
        match queued_selector_line(status)
            .unwrap_or_else(|| answer(false, editor.read_line(&prompt)))
        {
            Line::HostCommand(HostCommand::QuestionSelector(command)) => {
                let Some(key) = next_selector_key(status, &command) else {
                    break PermissionMenuChoice::Cancelled;
                };
                let mut selector = selector.lock().expect("permission selector lock");
                selector.key(key);
                if key == SelectorKey::Escape {
                    break PermissionMenuChoice::Cancelled;
                }
                if key != SelectorKey::Enter {
                    drop(selector);
                    repaint_status(status);
                    continue;
                }
                let Some(SelectorValue::Single(id)) = selector.result() else {
                    continue;
                };
                let Some(choice) = choices
                    .iter()
                    .copied()
                    .find(|choice| choice.as_str() == id.as_str())
                else {
                    break PermissionMenuChoice::Cancelled;
                };
                break PermissionMenuChoice::Selected(choice);
            }
            line @ (Line::Ended | Line::ExternalBreak(_)) => {
                break PermissionMenuChoice::Propagate(line);
            }
            Line::Submitted(_)
            | Line::Blank
            | Line::Interrupted
            | Line::Refused(_)
            | Line::HostCommand(_) => break PermissionMenuChoice::Cancelled,
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

    let choice = match choice {
        PermissionMenuChoice::Selected(choice) => choice,
        PermissionMenuChoice::Cancelled => return None,
        PermissionMenuChoice::Propagate(line) => return Some(line),
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
    let saved = enter_modal(editor, phase, status);
    status
        .lock()
        .expect("surface status lock")
        .question_selector = Some(Arc::clone(&selector));
    repaint_status(status);
    let result = loop {
        let question = Question::new(
            question_selector_prompt(&request, &selector),
            Arc::clone(status),
        );
        match queued_selector_line(status)
            .unwrap_or_else(|| answer(false, editor.read_line(&question)))
        {
            Line::HostCommand(HostCommand::QuestionSelector(command)) => {
                let Some(key) = next_selector_key(status, &command) else {
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
            Line::Submitted(_)
            | Line::Blank
            | Line::Interrupted
            | Line::Refused(_)
            | Line::HostCommand(_) => break PermissionField::Cancelled,
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
    let mut controller = PolicyControlController::new();
    let modes = controller.mode_options();
    let request = QuestionRequest::new(
        "Choose execution mode",
        QuestionMode::Single,
        modes
            .iter()
            .map(|mode| QuestionOption::new(mode.as_str(), format!("{mode:?}")))
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
    let saved = enter_modal(editor, phase, status);
    status
        .lock()
        .expect("surface status lock")
        .question_selector = Some(Arc::clone(&selector));
    repaint_status(status);

    let choice = loop {
        let prompt = Question::new(
            question_selector_prompt(&request, &selector),
            Arc::clone(status),
        );
        match queued_selector_line(status)
            .unwrap_or_else(|| answer(false, editor.read_line(&prompt)))
        {
            Line::HostCommand(HostCommand::QuestionSelector(command)) => {
                let Some(key) = next_selector_key(status, &command) else {
                    break PolicyModeChoice::Cancelled;
                };
                let mut selector = selector.lock().expect("policy mode selector lock");
                selector.key(key);
                if key == SelectorKey::Escape {
                    break PolicyModeChoice::Cancelled;
                }
                if key != SelectorKey::Enter {
                    drop(selector);
                    repaint_status(status);
                    continue;
                }
                let Some(SelectorValue::Single(id)) = selector.result() else {
                    continue;
                };
                let Some(mode) = modes
                    .iter()
                    .copied()
                    .find(|mode| mode.as_str() == id.as_str())
                else {
                    break PolicyModeChoice::Cancelled;
                };
                break PolicyModeChoice::Selected(mode);
            }
            Line::ExternalBreak(buffer) => {
                break PolicyModeChoice::Propagate(Line::ExternalBreak(buffer));
            }
            Line::Submitted(_)
            | Line::Blank
            | Line::Interrupted
            | Line::Ended
            | Line::Refused(_)
            | Line::HostCommand(_) => break PolicyModeChoice::Cancelled,
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

    match choice {
        PolicyModeChoice::Selected(mode) => controller
            .select_mode(mode)
            .map(|request| Line::HostCommand(HostCommand::Policy(request))),
        PolicyModeChoice::Cancelled => None,
        PolicyModeChoice::Propagate(line) => Some(line),
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
    Selected(Vec<ego_core::config::StatusSegmentName>),
    Cancelled,
    Propagate(Line),
}

/// The segments each `/statusline` preset writes, and the label that offers it.
///
/// One table rather than two, because a preset whose label disagrees with what
/// it writes is a lie the operator only finds out about after saving.
pub(super) const STATUSLINE_PRESETS: [(&str, &str, &[ego_core::config::StatusSegmentName]); 4] = {
    use ego_core::config::StatusSegmentName as Segment;

    [
        ("off", "Off", &[]),
        (
            "minimal",
            "Minimal — model, effort, mode",
            &[Segment::Model, Segment::Effort, Segment::Mode],
        ),
        (
            "default",
            "Default — model, effort, mode, context, cost, queue, drafts, interrupt, cwd, git, trust",
            DEFAULT_STATUSLINE_SEGMENTS,
        ),
        (
            "full",
            "Full — model, effort, mode, context, cost, queue, drafts, interrupt, cwd, git, trust",
            &[
                Segment::Model,
                Segment::Effort,
                Segment::Mode,
                Segment::Context,
                Segment::Cost,
                Segment::Queue,
                Segment::Drafts,
                Segment::Interrupt,
                Segment::Cwd,
                Segment::Git,
                Segment::Trust,
            ],
        ),
    ]
};

pub(super) fn choose_statusline_preset(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
) -> StatuslineChoice {
    let request = QuestionRequest::new(
        "Choose status-line setup",
        QuestionMode::Single,
        STATUSLINE_PRESETS
            .iter()
            .map(|(id, label, _)| QuestionOption::new(*id, *label))
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
    let saved = enter_modal(editor, phase, status);
    status
        .lock()
        .expect("surface status lock")
        .question_selector = Some(Arc::clone(&selector));
    repaint_status(status);

    let choice = loop {
        let prompt = Question::new(
            question_selector_prompt(&request, &selector),
            Arc::clone(status),
        );
        match queued_selector_line(status)
            .unwrap_or_else(|| answer(false, editor.read_line(&prompt)))
        {
            Line::HostCommand(HostCommand::QuestionSelector(command)) => {
                let Some(key) = next_selector_key(status, &command) else {
                    continue;
                };
                let mut selector = selector.lock().expect("status-line selector lock");
                selector.key(key);
                if key == SelectorKey::Escape {
                    break StatuslineChoice::Cancelled;
                }
                if key != SelectorKey::Enter {
                    drop(selector);
                    repaint_status(status);
                    continue;
                }
                let Some(SelectorValue::Single(id)) = selector.result() else {
                    continue;
                };
                let Some((_, _, segments)) = STATUSLINE_PRESETS
                    .iter()
                    .find(|(preset, _, _)| *preset == id.as_str())
                else {
                    continue;
                };
                break StatuslineChoice::Selected(segments.to_vec());
            }
            Line::Interrupted | Line::Blank => break StatuslineChoice::Cancelled,
            Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                break StatuslineChoice::Cancelled;
            }
            line @ (Line::Ended | Line::ExternalBreak(_)) => {
                break StatuslineChoice::Propagate(line);
            }
            Line::Submitted(_) | Line::Refused(_) | Line::HostCommand(_) => {}
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
    choice
}

pub(super) fn configure_statusline(
    status: &SharedStatus,
    segments: Vec<ego_core::config::StatusSegmentName>,
) {
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
    let names = segments
        .iter()
        .map(|segment| segment.as_str().to_string())
        .collect::<Vec<_>>();
    match store.mutate(UserConfigMutation::Statusline { segments: names }) {
        Ok(result) => {
            let mut current = status.lock().expect("surface status lock");
            current.configured = segments.into_iter().map(StatusSegmentName::from).collect();
            current.native_status = !current.configured.is_empty();
            current.notice = Some(match result.publication {
                UserConfigPublication::NotPublished if current.native_status => {
                    "status line already enabled; configuration unchanged".to_string()
                }
                UserConfigPublication::NotPublished => {
                    "status line already disabled; configuration unchanged".to_string()
                }
                UserConfigPublication::PublishedDurably if current.native_status => {
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
