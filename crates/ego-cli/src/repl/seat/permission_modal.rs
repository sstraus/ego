//! What the permission modal says.
//!
//! Pure text, so the wording can be tested without a terminal. The compact
//! prompt is what an operator needs to decide; everything the policy engine
//! knows about the call sits behind the details key.

use ego_core::permit::analysis::{RiskAssessment, RiskClass, RiskReason};
use ego_core::permit::shell::{ReadOnlyRefusal, read_only_refusal as read_only_refusal_of};
use ego_core::permit::{Op, PermissionDecision, PermissionRequest, SessionProcessAction};
use ego_core::permit::{shell_word, terminal_safe as safe};

/// A command longer than this is cut in the compact prompt so it stays one
/// row on a normal terminal; the details show it whole.
const COMMAND_CAP: usize = 100;

/// The same for the rule list on the `a` row, which also carries the key text.
const RULE_CAP: usize = 60;

/// What, how risky, and the keys. Remembered authority is shown in full,
/// wrapping to the terminal width, before the operator can save it.
pub(super) fn compact_prompt(request: &PermissionRequest, width: usize) -> String {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let prefix = request
        .child_name()
        .map_or_else(String::new, |name| format!("Child {}: ", safe(name)));
    let mut operations = request
        .ops()
        .iter()
        .map(|op| {
            let description = describe_operation(op);
            let (verb, target) = description.split_once(' ').unwrap_or((&description, ""));
            let room = width.saturating_sub(prefix.len() + verb.len() + 9);
            let target = if matches!(op, Op::Spawn { .. }) {
                crate::render::line::Line::plain(target)
                    .truncate_to(room)
                    .text()
            } else {
                keep_target(target, room)
            };
            format!("{verb} {target}")
        })
        .collect::<Vec<_>>()
        .join("; ");
    if request.ops().is_empty() {
        operations = safe(request.tool());
    }
    rows.push(format!("{prefix}Allow {operations}?"));
    rows.extend(headline_rows(request).into_iter().skip(1));
    rows.push(if let Some(rules) = remembered_rules(request) {
        format!("[y] yes   [a] yes, do not ask again for {}", safe(&rules))
    } else {
        "[y] yes".into()
    });
    rows.push("[n] no, say what to do instead   [e] edit   [d] details ".into());
    rows.into_iter()
        .flat_map(|row| {
            if row.is_empty() {
                vec![String::new()]
            } else {
                crate::render::line::Line::plain(row)
                    .wrap(width)
                    .into_iter()
                    .map(|line| line.text())
                    .collect()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Keep the operation target's suffix, measured in display cells, not bytes.
fn keep_target(text: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    if text.width() <= width {
        return text.to_owned();
    }
    let mut kept = Vec::new();
    let mut cells = 1;
    for grapheme in text.graphemes(true).rev() {
        if cells + grapheme.width() > width {
            break;
        }
        cells += grapheme.width();
        kept.push(grapheme);
    }
    kept.reverse();
    format!("…{}", kept.concat())
}

/// The compact prompt's facts plus everything behind the details key: the
/// reason, the canonical operations, the exact parameters, and what remembering
/// would store and widen.
pub(super) fn details_prompt(
    request: &PermissionRequest,
    capabilities: crate::render::Capabilities,
) -> String {
    let mut rows = headline_rows(request);
    // The headline rows cut a long judge reason; the details show it whole.
    if let Some(reason) = request
        .presentation()
        .and_then(|presentation| presentation.judge_reason())
        .filter(|reason| reason.chars().count() > COMMAND_CAP)
    {
        rows.push(format!("Judge reason: {}", safe(reason)));
    }
    rows.push(format!("Tool: {}", safe(request.tool())));
    rows.push(format!("Reason: {}", safe(&request.why().to_string())));
    let operations = request
        .ops()
        .iter()
        .map(|op| match op {
            Op::Write { path, change } => format!(
                "- write {}\n{}",
                serde_json::to_string(&path.to_string()).expect("a path serializes"),
                crate::render::permission_diff::render(change, capabilities)
            ),
            _ => PermissionRequest::new(request.tool(), vec![op.clone()], request.why().clone())
                .to_string()
                .lines()
                .skip(1)
                .collect::<Vec<_>>()
                .join("\n"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !operations.is_empty() {
        rows.push(format!("Operations:\n{operations}"));
    }
    if let Some(presentation) = request.presentation() {
        let mut parameters = presentation.parameters().clone();
        if request
            .ops()
            .iter()
            .any(|op| matches!(op, Op::Write { .. }))
            && matches!(request.tool(), "edit" | "write")
            && let Some(fields) = parameters.as_object_mut()
        {
            for name in ["old_string", "new_string", "contents"] {
                if let Some(value) = fields.get_mut(name) {
                    *value = serde_json::Value::String("[shown in bounded diff]".into());
                }
            }
        }
        rows.push(format!(
            "Parameters:\n{}",
            serde_json::to_string_pretty(&parameters).expect("JSON parameters always serialize")
        ));
        match presentation.suggested_remember_plan() {
            Ok(plan) => {
                rows.push("Remembering would add:".into());
                rows.extend(
                    plan.candidate_rows()
                        .map(|(operation, rule)| format!("- operation {operation}: {rule}")),
                );
                for (operation, dimension) in plan.omission_rows() {
                    rows.push(format!(
                        "- widening for operation {operation}: omits {dimension:?}"
                    ));
                }
            }
            Err(error) => rows.push(format!("Remembering is unavailable: {error}")),
        }
    }
    rows.extend(key_rows(request, false));
    rows.join("\n")
}

/// The one row a settled approval leaves in the transcript: what was asked,
/// how it was answered, and the answer's own notice (a remembered rule, an
/// interruption), if any.
pub(super) fn settled_receipt(
    request: &PermissionRequest,
    decision: &PermissionDecision,
    notice: Option<&str>,
) -> String {
    let mut call = if request.ops().is_empty() {
        safe(request.tool())
    } else {
        truncate(&describe_operations(request.ops()), COMMAND_CAP)
    };
    if let Some(name) = request.child_name() {
        call = format!("child {}: {call}", safe(name));
    }
    let mut row = match decision {
        PermissionDecision::Allow => format!("Allowed {call}"),
        PermissionDecision::Amend { .. } => format!("Edited {call}"),
        PermissionDecision::Deny => format!("Denied {call}"),
        PermissionDecision::DenyWithInstruction { instruction } => {
            format!("Denied {call}; instead: {}", safe(instruction))
        }
    };
    if let Some(notice) = notice {
        row.push_str(" · ");
        row.push_str(&safe(notice));
    }
    row
}

fn headline_rows(request: &PermissionRequest) -> Vec<String> {
    let mut rows = vec![format!(
        "Allow {}?",
        truncate(&describe_operations(request.ops()), COMMAND_CAP)
    )];
    if let Some(name) = request.child_name() {
        rows[0] = format!("Child {}: {}", safe(name), rows[0]);
    }
    if let Some(presentation) = request.presentation() {
        let mut risk = risk_words(presentation.risk());
        if let Some(reason) = read_only_refusal(request.ops()) {
            risk.push_str("; not read-only: ");
            risk.push_str(reason);
        }
        rows.push(risk);
        if let Some(reason) = presentation.judge_reason() {
            rows.push(format!("Judge: {}", truncate(&safe(reason), COMMAND_CAP)));
        }
    }
    rows
}

/// Why the shell read-only proof refuses a program this call runs, in plain
/// words. The risk class stays what the policy engine assessed.
fn read_only_refusal(ops: &[Op]) -> Option<&'static str> {
    ops.iter().find_map(|op| {
        let Op::Spawn { spec } = op else {
            return None;
        };
        let argv: Vec<_> = spec
            .argv()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        Some(match read_only_refusal_of(&argv)? {
            ReadOnlyRefusal::RepositoryHelpers => "git may run repository-configured commands",
            ReadOnlyRefusal::ImplicitSearch => "the command does not name every file it searches",
        })
    })
}

fn key_rows(request: &PermissionRequest, with_details: bool) -> Vec<String> {
    let mut yes = "[y] yes".to_owned();
    if let Some(rules) = remembered_rules(request) {
        yes.push_str(&format!(
            "   [a] yes, do not ask again for {}",
            truncate(&rules, RULE_CAP)
        ));
    }
    let mut no = "[n] no, say what to do instead   [e] edit".to_owned();
    if with_details {
        no.push_str("   [d] details");
    }
    // The trailing space is where the cursor sits.
    no.push(' ');
    vec![yes, no]
}

/// The rules `a` would store, in the words the rule store prints them.
fn remembered_rules(request: &PermissionRequest) -> Option<String> {
    let plan = request.presentation()?.suggested_remember_plan().ok()?;
    Some(
        plan.candidates
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn describe_operations(ops: &[Op]) -> String {
    ops.iter()
        .map(describe_operation)
        .collect::<Vec<_>>()
        .join("; ")
}

fn describe_operation(op: &Op) -> String {
    match op {
        Op::Read { path } => format!("read {}", safe(&path.to_string())),
        Op::List { root, .. } => format!("list {}", safe(&root.to_string())),
        Op::Search { root, query } => {
            format!("search {} for {}", safe(&root.to_string()), safe(query))
        }
        Op::Write { path, .. } => format!("write {}", safe(&path.to_string())),
        Op::Spawn { spec } => format!(
            "run {}",
            spec.argv()
                .iter()
                .map(|argument| shell_word(&argument.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Op::ReadEvidence { .. } => "read saved tool output".to_owned(),
        Op::SessionProcess { id, action } => {
            let verb = match action {
                SessionProcessAction::Inspect => "inspect",
                SessionProcessAction::Cancel => "cancel",
            };
            format!("{verb} session process {id}")
        }
        Op::Opaque { tool, .. } => format!("call {}", safe(&tool.to_string())),
        Op::Connect { target, .. } | Op::ProtocolConnect { target } => {
            format!("connect to {}", safe(&target.to_string()))
        }
        Op::SecretEgress { authority } => {
            format!("send secrets to {}", safe(&authority.target().to_string()))
        }
    }
}

/// One row, in plain words. The class word differs per class so two
/// classifications never read alike; the reasons say what the engine saw.
fn risk_words(risk: &RiskAssessment) -> String {
    let class = match risk.class {
        RiskClass::Low => "low",
        RiskClass::Bounded => "limited",
        RiskClass::Unknown => "unknown",
        RiskClass::High => "high",
    };
    let reasons = risk
        .reasons
        .iter()
        .map(|reason| match reason {
            RiskReason::ReadOnly => "only reads",
            RiskReason::SessionProcessControl => "controls a session process",
            RiskReason::ScopedWrite => "changes files",
            RiskReason::WorkspaceWrite => "changes files in the workspace",
            RiskReason::OutsideWorkspaceWrite => "changes files outside the workspace",
            RiskReason::ProcessEffectsUnknown => "runs a program",
            RiskReason::IdentityIncomplete => "program not fully identified",
            RiskReason::DynamicCode => "runs code from its arguments",
            RiskReason::ForeignEffectsUnknown => "calls an outside tool",
            RiskReason::NetworkEffectsUnknown => "uses the network",
            RiskReason::SecretRelease => "sends a secret",
            RiskReason::OrchestrationPolicy => "coordinator operation",
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("Risk: {class} - {reasons}")
}

fn truncate(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_owned();
    }
    let kept: String = text.chars().take(cap.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: fixed character limits hide the target and wrap decision keys.
    #[cfg(unix)]
    #[test]
    fn rendered_permission_retains_target_at_80_columns() {
        use ego_core::permit::{CanonPath, Change, Escalation, OpName};
        use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
        use std::io::Read;
        for target in ["review-example.txt", "界面-example.txt"] {
            let path = CanonPath::new(format!(
                "/workspace/{}/{target}",
                "long-directory/".repeat(12)
            ));
            let request = PermissionRequest::new(
                "write",
                vec![Op::Write {
                    path: path.clone(),
                    change: Change::Whole(b"hello".to_vec()),
                }],
                Escalation::NoRuleCovers {
                    op: OpName::Write,
                    path: path.clone(),
                },
            );
            for width in [80, 120, 200] {
                let prompt = compact_prompt(&request, width);
                let pair = NativePtySystem::default()
                    .openpty(PtySize {
                        rows: 24,
                        cols: width as u16,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .unwrap();
                let mut command = CommandBuilder::new("/bin/sh");
                command.args(["-c", "printf '%s' \"$1\"", "permission-capture", &prompt]);
                let mut child = pair.slave.spawn_command(command).unwrap();
                drop(pair.slave);
                let mut bytes = Vec::new();
                let mut reader = pair.master.try_clone_reader().unwrap();
                let mut buffer = [0; 4096];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                        Err(error) if error.raw_os_error() == Some(5) => break,
                        Err(error) => panic!("PTY read: {error}"),
                    }
                }
                assert!(child.wait().unwrap().success());
                let mut terminal = vt100::Parser::new(24, width as u16, 0);
                terminal.process(&bytes);
                let grid = terminal.screen().contents();
                assert!(
                    grid.lines().any(|row| row.starts_with("Allow write ")
                        && row.ends_with(&format!("{target}?"))),
                    "{width}: {grid}"
                );
                for key in ["[y]", "[n]", "[e]", "[d]"] {
                    assert!(grid.contains(key), "{grid}");
                }
                assert!(
                    prompt
                        .lines()
                        .all(|row| unicode_width::UnicodeWidthStr::width(row) <= width)
                );
                let detail = details_prompt(&request, crate::render::Capabilities::plain());
                assert!(detail.contains(&path.to_string()));
                println!("permission width={width} grid={grid:?}");
            }
        }
    }

    // Catches: long exec arguments hide the executable or alter the settled decision.
    #[test]
    fn permission_width_preserves_exec_and_settled_decisions() {
        use ego_core::config::Mode;
        use ego_core::permit::{CanonPath, EnvPolicy, Escalation, OpName, SpawnSpec};
        let argument = "long argument ".repeat(40);
        let spec = SpawnSpec::new(
            vec!["/bin/echo".into(), argument.clone().into()],
            None,
            None,
            EnvPolicy::Inherited,
        )
        .unwrap();
        let request = PermissionRequest::new(
            "exec",
            vec![Op::Spawn { spec }],
            Escalation::ModeAsks {
                mode: Mode::Default,
                op: OpName::Spawn,
                path: CanonPath::new("/workspace"),
            },
        );
        for width in [80, 120, 200] {
            let prompt = compact_prompt(&request, width);
            assert!(prompt.contains("Allow run /bin/echo"), "{prompt}");
            for key in ["[y]", "[n]", "[e]", "[d]"] {
                assert!(prompt.contains(key));
            }
            assert!(
                prompt
                    .lines()
                    .all(|line| unicode_width::UnicodeWidthStr::width(line) <= width)
            );
        }
        let details = details_prompt(&request, crate::render::Capabilities::plain());
        assert!(details.contains(&argument));
        assert!(
            settled_receipt(&request, &PermissionDecision::Allow, None)
                .starts_with("Allowed run /bin/echo")
        );
        assert!(
            settled_receipt(&request, &PermissionDecision::Deny, None)
                .starts_with("Denied run /bin/echo")
        );
    }

    fn risk(class: RiskClass, reasons: Vec<RiskReason>) -> RiskAssessment {
        RiskAssessment { class, reasons }
    }

    // Catches: an external write retaining the misleading workspace wording.
    #[test]
    fn outside_workspace_write_risk_names_the_location() {
        assert_eq!(
            risk_words(&risk(
                RiskClass::Bounded,
                vec![RiskReason::OutsideWorkspaceWrite]
            )),
            "Risk: limited - changes files outside the workspace"
        );
        assert_eq!(
            risk_words(&risk(RiskClass::Bounded, vec![RiskReason::ScopedWrite])),
            "Risk: limited - changes files"
        );
    }

    /// Catches the classifications collapsing into one message, and Rust
    /// variant names leaking into the words.
    #[test]
    fn bounded_and_unknown_risk_read_differently_in_plain_words() {
        let bounded = risk_words(&risk(RiskClass::Bounded, vec![RiskReason::WorkspaceWrite]));
        let unknown = risk_words(&risk(
            RiskClass::Unknown,
            vec![RiskReason::ProcessEffectsUnknown, RiskReason::DynamicCode],
        ));
        assert_eq!(bounded, "Risk: limited - changes files in the workspace");
        assert_eq!(
            unknown,
            "Risk: unknown - runs a program, runs code from its arguments"
        );
        for words in [&bounded, &unknown] {
            assert!(
                !words.contains("Effects") && !words.contains("Scoped"),
                "{words}"
            );
        }
    }

    /// Catches detached process controls losing their target or presenting
    /// cancellation as a read-only operation in the permission modal.
    #[test]
    fn session_process_controls_show_the_action_target_in_the_rendered_prompt() {
        use ego_core::config::Mode;
        use ego_core::permit::{CanonPath, Escalation, OpName};
        let id = ego_core::event::id::DetachedExecId::new();
        for (action, verb) in [
            (SessionProcessAction::Inspect, "inspect"),
            (SessionProcessAction::Cancel, "cancel"),
        ] {
            let request = PermissionRequest::new(
                "exec_control",
                vec![Op::SessionProcess { id, action }],
                Escalation::ModeAsks {
                    mode: Mode::Default,
                    op: OpName::SessionProcess,
                    path: CanonPath::new("/workspace"),
                },
            );
            let rendered = compact_prompt(&request, 120);
            assert!(
                rendered.contains(&format!("Allow {verb} session process {id}?")),
                "{rendered}"
            );
            assert!(rendered.contains("[y] yes"), "{rendered}");
            assert!(rendered.contains("[n] no"), "{rendered}");
        }
    }

    // Catches: bidi argv reorders the approval prompt and tool row visually.
    #[test]
    fn bidi_argv_is_escaped_in_real_permission_prompt_and_tool_row() {
        use ego_core::config::Mode;
        use ego_core::permit::{CanonPath, EnvPolicy, Escalation, OpName, SpawnSpec};
        let hostile = "a\u{202e}txt";
        let spec = SpawnSpec::new(
            vec!["/bin/echo".into(), hostile.into()],
            None,
            None,
            EnvPolicy::Inherited,
        )
        .unwrap();
        let request = PermissionRequest::new(
            "exec",
            vec![Op::Spawn { spec }],
            Escalation::ModeAsks {
                mode: Mode::Default,
                op: OpName::Spawn,
                path: CanonPath::new("/workspace"),
            },
        );
        let prompt = compact_prompt(&request, 120);
        let args = serde_json::json!({"argv": ["/bin/echo", hostile]}).to_string();
        let tool = crate::render::tool_row::record(
            &crate::render::tool_row::Finished {
                name: "exec",
                args: &args,
                ok: true,
                result: hostile,
                timing: None,
            },
            crate::render::Capabilities::plain(),
        );
        for text in [&prompt, &tool.rows, &tool.detail] {
            assert!(text.contains("\\u{202e}"), "unescaped bidi: {text:?}");
            assert!(!text.contains('\u{202e}'), "active bidi: {text:?}");
        }
    }

    /// Catches a command shown differently from the one that would run: an
    /// argument with a space or a quote must read back as one argument.
    #[test]
    fn shell_words_quote_what_a_shell_would_split() {
        assert_eq!(shell_word("print"), "print");
        assert_eq!(shell_word("print(1); x"), "'print(1); x'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word(""), "''");
    }

    /// Catches a control character in a command repainting the modal.
    #[test]
    fn controls_are_shown_and_never_sent() {
        assert_eq!(safe("a\u{1b}[2Jb\n"), "a\\u{1b}[2Jb\\n");
        assert_eq!(truncate(&"x".repeat(120), 100).chars().count(), 100);
        assert_eq!(truncate("short", 100), "short");
    }
}

#[cfg(test)]
mod permission_modal_diff_tests {
    use super::*;
    use crate::render::caps::{Capabilities, Terminal};
    use ego_core::permit::{CanonPath, Change, Escalation, Op, OpName};

    fn request(change: Change) -> PermissionRequest {
        let path = CanonPath::new("/workspace/new.py");
        PermissionRequest::new(
            "edit",
            vec![Op::Write {
                path: path.clone(),
                change,
            }],
            Escalation::NoRuleCovers {
                op: OpName::Write,
                path,
            },
        )
    }

    #[test]
    fn permission_modal_edit_diff_catches_escaped_string_dump() {
        let request = request(Change::Replace {
            replace_all: false,
            old_string: "def subtract(a, b):\n    return a - b\n".into(),
            new_string: "def subtract(a, b):\n    return b - a\n".into(),
        });
        let out = details_prompt(&request, Capabilities::plain());
        assert!(
            out.contains(
                "@@ -1,2 +1,2 @@\n def subtract(a, b):\n-    return a - b\n+    return b - a"
            ),
            "{out}"
        );
        assert!(!out.contains(r"\n"), "{out}");
        assert!(out.contains("fragment"));
        assert!(out.ends_with("[n] no, say what to do instead   [e] edit "));
        let compact = compact_prompt(&request, 120);
        assert!(!compact.contains("@@"));
        assert!(!compact.contains("return a - b"));
        assert!(compact.lines().count() <= 5);
    }

    #[test]
    fn permission_modal_colour_catches_escapes_in_plain_output() {
        let request = request(Change::Replace {
            replace_all: false,
            old_string: "old\n".into(),
            new_string: "new\n".into(),
        });
        for (tty, no_color, term, flag, coloured) in [
            (true, None, "xterm-256color", false, true),
            (false, None, "xterm-256color", false, false),
            (true, Some("1"), "xterm-256color", false, false),
            (true, Some("0"), "xterm-256color", false, false),
            (true, None, "dumb", false, false),
            (true, None, "xterm-256color", true, false),
        ] {
            let caps = Capabilities::of(&Terminal {
                stdout_is_terminal: tty,
                no_color,
                term: Some(term),
                flag_no_color: flag,
                ..Terminal::default()
            });
            let out = details_prompt(&request, caps);
            assert_eq!(
                out.contains("\x1b["),
                coloured,
                "{tty} {no_color:?} {term} {flag}: {out:?}"
            );
        }
    }

    #[test]
    fn permission_modal_write_diff_catches_missing_additions_and_unbounded_output() {
        let out = details_prompt(
            &request(Change::Whole(b"first\nsecond\n".to_vec())),
            Capabilities::plain(),
        );
        assert!(out.contains("@@ -0,0 +1,2 @@\n+first\n+second"), "{out}");
        assert!(out.contains("previous content is not read"));
        let content = (0..100).map(|n| format!("line {n}\n")).collect::<String>();
        let out = details_prompt(
            &request(Change::Whole(content.into_bytes())),
            Capabilities::plain(),
        );
        // A unified diff has two headers, one hunk header and 100 addition rows.
        assert!(out.contains("[73 diff lines omitted]"), "{out}");
        assert!(!out.contains("+line 99"));
        assert!(out.lines().count() < 45, "{out}");
    }

    #[test]
    fn permission_modal_diff_catches_terminal_controls_and_overwide_lines() {
        let out = details_prompt(
            &request(Change::Whole(
                format!("\x1b[2J{}\n", "界".repeat(1000)).into_bytes(),
            )),
            Capabilities::plain(),
        );
        assert!(!out.contains('\x1b'));
        assert!(out.contains('�'));
        assert!(out.len() < 2000);
    }
}

#[cfg(test)]
mod sandbox_grant_tests {
    use super::*;
    // Catches: a sandbox question renders as "Allow ?", hides its path/retry,
    // or offers a persisted permission rule instead of session-only authority.
    #[test]
    fn sandbox_prompt_names_session_path_and_retry_without_remembering() {
        let grant = ego_core::permit::SandboxGrant::new(
            ego_core::permit::CanonPath::new("/outside/input"),
            ego_core::permit::SandboxAccess::Read,
        );
        let request = PermissionRequest::sandbox_grant(&grant, "/bin/cat /outside/input");
        let prompt = compact_prompt(&request, 160);
        for text in [
            "read /outside/input",
            "for this session",
            "will run again",
            "/bin/cat",
            "[y] yes",
        ] {
            assert!(prompt.contains(text), "{prompt}");
        }
        assert!(!prompt.contains("[a]"), "{prompt}");
        assert!(request.requires_explicit_answer());
    }
}
