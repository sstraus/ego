//! The terminal report a managed peer owes its parent.
//!
//! A host that spawns ego as a managed peer states the contract in the brief:
//! the peer "must report task completion or a real blocker with" an
//! `agent action=send to="<parent>"` call. A turn that ends without that call
//! leaves the parent waiting on a peer that believes it is done. The contract
//! is read from the brief text and the report is recognised by admitted tool
//! result identity, so no per-brief wording is hard-coded here.

use crate::permit::ToolResultProvenance;
use crate::state::{ItemKind, ItemState, SessionState};

/// How many times one session reminds the peer before it ends normally.
const REMINDER_LIMIT: usize = 2;

const CONTRACT_MARKER: &str = "must report";
const SEND_PREFIX: &str = "agent action=send to=\"";

/// The parent named by the managed brief contract, if the session has one.
fn parent_of(brief: &str) -> Option<String> {
    let after = &brief[brief.find(CONTRACT_MARKER)? + CONTRACT_MARKER.len()..];
    let rest = &after[after.find(SEND_PREFIX)? + SEND_PREFIX.len()..];
    let parent = &rest[..rest.find('"')?];
    (!parent.is_empty()).then(|| parent.to_owned())
}

fn is_report_to(item: &ItemState, parent: &str) -> bool {
    if item.kind != ItemKind::ToolCall || item.ok != Some(true) {
        return false;
    }
    let Some(result) = item.tool_result.as_ref() else {
        return false;
    };
    let ToolResultProvenance::Foreign { result } = result.provenance() else {
        return false;
    };
    if result.server() != "tuic" {
        return false;
    }
    let Ok(args) = serde_json::from_str::<serde_json::Value>(&item.args) else {
        return false;
    };
    let call = match result.tool() {
        "agent" => &args,
        "call_tool" if args["tool_name"] == "agent" => &args["arguments"],
        _ => return false,
    };
    let message = call["message"].as_str().unwrap_or_default().trim_start();
    call["action"] == "send"
        && call["to"] == parent
        && ["RESULT", "BLOCKED"].iter().any(|word| {
            message
                .get(..word.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(word))
        })
}

fn reminder_text(parent: &str) -> String {
    format!(
        "Your brief requires a terminal report to your parent, and none was sent. \
         Send it now with `agent action=send to=\"{parent}\"`, with a message that \
         starts with RESULT, or with BLOCKED if you cannot proceed. Then end your turn."
    )
}

/// The reminder to run as the next turn, when the session is a managed peer
/// that has not reported and has not used up its reminders.
pub(super) fn missing_report(state: &SessionState) -> Option<String> {
    let mut prompts: Vec<&ItemState> = state
        .items
        .values()
        .filter(|item| item.kind == ItemKind::UserPrompt)
        .collect();
    prompts.sort_by_key(|item| item.seq);
    let parent = prompts.iter().find_map(|item| parent_of(&item.text))?;
    if state.items.values().any(|item| is_report_to(item, &parent)) {
        return None;
    }
    let text = reminder_text(&parent);
    let sent = prompts.iter().filter(|item| item.text == text).count();
    (sent < REMINDER_LIMIT).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIEF: &str = "## Your Task\n\nYou can communicate with your parent at any time, and must report task\ncompletion or a real blocker with:\n`agent action=send to=\"c8980cac\" message=\"<done summary>\"`\n";

    // Catches: the parent is hard-coded or read from the wrong quoted id.
    #[test]
    fn parent_is_read_from_the_contract() {
        assert_eq!(parent_of(BRIEF).as_deref(), Some("c8980cac"));
        assert_eq!(
            parent_of("agent action=send to=\"x\" with no contract"),
            None
        );
        assert_eq!(
            parent_of("must report with agent action=send to=\"\""),
            None
        );
    }
}
