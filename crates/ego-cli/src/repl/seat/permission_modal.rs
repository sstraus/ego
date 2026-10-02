//! What the permission modal says.
//!
//! Pure text, so the wording can be tested without a terminal. The compact
//! prompt is what an operator needs to decide; everything the policy engine
//! knows about the call sits behind the details key.

use ego_core::permit::analysis::{RiskAssessment, RiskClass, RiskReason};
use ego_core::permit::{Op, PermissionRequest};

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
pub(super) fn details_prompt(request: &PermissionRequest) -> String {
    let mut rows = headline_rows(request);
    rows.push(format!("Tool: {}", safe(request.tool())));
    rows.push(format!("Reason: {}", safe(&request.why().to_string())));
    let rendered = request.to_string();
    let operations = rendered.lines().skip(1).collect::<Vec<_>>().join("\n");
    if !operations.is_empty() {
        rows.push(format!("Operations:\n{operations}"));
    }
    if let Some(presentation) = request.presentation() {
        rows.push(format!(
            "Parameters:\n{}",
            serde_json::to_string_pretty(presentation.parameters())
                .expect("JSON parameters always serialize")
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

fn headline_rows(request: &PermissionRequest) -> Vec<String> {
    let mut rows = vec![format!(
        "Allow {}?",
        truncate(&describe_operations(request.ops()), COMMAND_CAP)
    )];
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
            RiskReason::ScopedWrite => "changes files in the workspace",
            RiskReason::ProcessEffectsUnknown => "runs a program",
            RiskReason::IdentityIncomplete => "program not fully identified",
            RiskReason::DynamicCode => "runs code from its arguments",
            RiskReason::ForeignEffectsUnknown => "calls an outside tool",
            RiskReason::NetworkEffectsUnknown => "uses the network",
            RiskReason::SecretRelease => "sends a secret",
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
