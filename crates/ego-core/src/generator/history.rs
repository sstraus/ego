//! Rebuilding a sendable [`Conversation`] from a reduced session log.
//!
//! See `docs/decisions/2026-08-21-tool-use-history.md` §4. This lives beside
//! [`Conversation`] rather than inside the turn loop because it is defined
//! entirely in terms of the invariants that type owns; the loop is one caller
//! of it, and putting it here means it is testable before the loop exists and
//! stays testable without one.
//!
//! The durable log is never rewritten. Everything below produces the request
//! that is about to be assembled, and nothing else.

use super::types::{ContentBlock, Conversation, Message, Role};
use crate::state::{ItemKind, ItemState, SessionState};

/// Something the rebuild had to do to make the history sendable.
///
/// Returned rather than logged: whether a silently repaired history is worth
/// telling the operator about is the caller's judgement, not this module's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repair {
    /// A call had no result — the session died between dispatching it and
    /// recording what came back — so one was synthesized. The call and the
    /// turn that made it both survive.
    AnsweredTornCall { call_id: String },
    /// A call carried no provider id, so it could not be sent at all and
    /// everything from it onwards was dropped. Only a record written before
    /// `ToolCallStarted` kept `call_id` can produce this.
    TruncatedUnidentifiedCall,
}

/// What a synthesized answer says. Deliberately not a tool's own vocabulary:
/// the tool did not say this, the harness did.
const TORN: &str = "the tool call did not complete: the session ended before its result was known";

/// Rebuild `state`'s history as something a provider will accept, and say
/// what had to be repaired to get there.
///
/// The result is always a valid [`Conversation`]: every branch either
/// completes the history or truncates to a prefix that was already complete.
pub fn conversation_from(state: &SessionState) -> (Conversation, Vec<Repair>) {
    let mut items: Vec<&ItemState> = state.items.values().collect();
    // By `seq`, not by `ItemId` — see `ItemState::seq`.
    items.sort_by_key(|item| item.seq);

    let mut messages: Vec<Message> = Vec::new();
    let mut repairs = Vec::new();
    // Calls this history is still waiting on. A `Vec` and not an `Option`:
    // one generation can dispatch several calls in parallel and the crash
    // that tore one of them can have torn all of them.
    let mut unanswered: Vec<String> = Vec::new();

    for item in items {
        match item.kind {
            // Never rebuilt as assistant text. No wire format this repo
            // implements can carry reasoning back — openai-completions
            // refuses `reasoning_state` outright — and promoting it to text
            // would put words in the model's mouth it never published.
            ItemKind::Reasoning => {}
            ItemKind::UserPrompt => {
                push(&mut messages, Role::User, text_block(&item.text));
            }
            ItemKind::Assistant => {
                push(&mut messages, Role::Assistant, text_block(&item.text));
            }
            ItemKind::ToolCall => {
                let Some(call_id) = item.call_id.clone() else {
                    // Unsendable, and so is everything after it: a result may
                    // only answer a call that appears earlier, so the calls
                    // that follow cannot be re-parented onto anything. Calls
                    // torn *before* this one are still answered by `finish`,
                    // because their turn is part of the surviving prefix.
                    repairs.push(Repair::TruncatedUnidentifiedCall);
                    let conversation = finish(messages, unanswered, &mut repairs);
                    return (conversation, repairs);
                };
                push(
                    &mut messages,
                    Role::Assistant,
                    ContentBlock::ToolUse {
                        call_id: call_id.clone(),
                        name: item.tool_name.clone().unwrap_or_default(),
                        args: item.args.clone(),
                    },
                );
                if item.completed {
                    push(
                        &mut messages,
                        Role::User,
                        ContentBlock::ToolResult {
                            call_id,
                            output: item.text.clone(),
                            // `ok` is absent only on a record that says a call
                            // completed without saying how, which is not a
                            // success anyone observed.
                            is_error: !item.ok.unwrap_or(false),
                        },
                    );
                } else {
                    unanswered.push(call_id);
                }
            }
        }
    }

    let conversation = finish(messages, unanswered, &mut repairs);
    (conversation, repairs)
}

/// Answer every outstanding call, then check the result.
fn finish(
    mut messages: Vec<Message>,
    unanswered: Vec<String>,
    repairs: &mut Vec<Repair>,
) -> Conversation {
    for call_id in unanswered {
        repairs.push(Repair::AnsweredTornCall {
            call_id: call_id.clone(),
        });
        push(
            &mut messages,
            Role::User,
            ContentBlock::ToolResult {
                call_id,
                output: TORN.to_string(),
                is_error: true,
            },
        );
    }
    Conversation::new(messages).expect(
        "the rebuild answers every call it emits, so the history it produces satisfies Conversation",
    )
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_string(),
    }
}

/// Append `block`, extending the last message when it has the same author.
///
/// Consecutive same-role items are one turn, not several: an assistant's text
/// and the calls it then made are one assistant turn, and grouping the
/// results of parallel calls into one user turn is what Anthropic requires.
fn push(messages: &mut Vec<Message>, role: Role, block: ContentBlock) {
    match messages.last_mut() {
        Some(last) if last.role == role => last.content.push(block),
        _ => messages.push(Message {
            role,
            content: vec![block],
        }),
    }
}
