//! What the permission modal says.
//!
//! Pure text, so the wording can be tested without a terminal. The compact
//! prompt is what an operator needs to decide; everything the policy engine
//! knows about the call sits behind the details key.

use ego_core::permit::analysis::{RiskAssessment, RiskClass, RiskReason};
use ego_core::permit::{Op, PermissionDecision, PermissionRequest, SessionProcessAction};

/// A command longer than this is cut in the compact prompt so it stays one
/// row on a normal terminal; the details show it whole.
const COMMAND_CAP: usize = 100;

/// The same for the rule list on the `a` row, which also carries the key text.
const RULE_CAP: usize = 60;

/// Four rows at most: what, how risky, and the keys.
pub(super) fn compact_prompt(request: &PermissionRequest) -> String {
    let mut rows = headline_rows(request);
    rows.extend(key_rows(request, true));
    rows.join("\n")
}

/// The compact prompt's facts plus everything behind the details key: the
/// reason, the canonical operations, the exact parameters, and what remembering
/// would store and widen.
pub(super) fn details_prompt(
    request: &PermissionRequest,
    capabilities: crate::render::Capabilities,
) -> String {
    let mut rows = headline_rows(request);
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
    let mut call = truncate(&describe_operations(request.ops()), COMMAND_CAP);
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
        rows.push(risk_words(presentation.risk()));
    }
    rows
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
            RiskReason::ScopedWrite => "changes files in the workspace",
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

/// A word a shell would read back as the same single argument.
fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        });
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", safe(&word.replace('\'', r"'\''")))
    }
}

/// Every control character shown, never executed.
fn safe(text: &str) -> String {
    text.chars()
        .flat_map(|character| {
            if character.is_control() {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
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

    fn risk(class: RiskClass, reasons: Vec<RiskReason>) -> RiskAssessment {
        RiskAssessment { class, reasons }
    }

    /// Catches the classifications collapsing into one message, and Rust
    /// variant names leaking into the words.
    #[test]
    fn bounded_and_unknown_risk_read_differently_in_plain_words() {
        let bounded = risk_words(&risk(RiskClass::Bounded, vec![RiskReason::ScopedWrite]));
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
            let rendered = compact_prompt(&request);
            assert!(
                rendered.contains(&format!("Allow {verb} session process {id}?")),
                "{rendered}"
            );
            assert!(rendered.contains("[y] yes"), "{rendered}");
            assert!(rendered.contains("[n] no"), "{rendered}");
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
        let compact = compact_prompt(&request);
        assert!(!compact.contains("@@"));
        assert!(!compact.contains("return a - b"));
        assert!(compact.lines().count() <= 4);
    }

    #[test]
    fn permission_modal_colour_catches_escapes_in_plain_output() {
        let request = request(Change::Replace {
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
