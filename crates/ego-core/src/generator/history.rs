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

use super::types::{ContentBlock, Conversation, HistoryError, Message, Role};
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
///
/// **The second sentence is `POTENTIALS.md` P21's answer.** P21's trigger was
/// "the first tool whose `descriptor` cannot honestly claim to be read-only",
/// and `write` is it. A tear is precisely the case where nobody knows whether
/// the call took effect, so repeating it and not repeating it are both capable
/// of being wrong — and the party best placed to find out is the one holding
/// the tools. Telling it "this did not complete" and nothing more is what
/// makes the repeat the default.
///
/// P21 sketches a bigger answer — a `safe_to_repeat` on `ToolDescriptor`, a
/// per-dispatch delivery certainty modelled on the transport's. None of that
/// is built, because all of it exists to decide *without* asking, and there is
/// a model in the loop here that can simply look. It becomes owed when
/// something has to resolve a tear with nobody to ask.
const TORN: &str = "the tool call did not complete: the session ended before its result was known. \
     It may already have taken effect. Check the current state of whatever it \
     would have changed before calling it again";

/// Rebuild `state`'s history as something a provider will accept, and say
/// what had to be repaired to get there.
///
/// Every branch either completes the history or truncates to a prefix that was
/// already complete, so no repair below can *cause* an `Err`. What it can do is
/// fail to cure one that was already in the log: a log holding two calls under
/// one provider id is corruption, not a tear, and it is refused rather than
/// given a reading (ADR §6). The whole [`HistoryError`] propagates and not just
/// that variant, because a rebuild that names the one error it believes
/// reachable is the exact claim §6 was written to retract.
pub fn conversation_from(
    state: &SessionState,
) -> Result<(Conversation, Vec<Repair>), HistoryError> {
    let mut items: Vec<&ItemState> = state.items.values().collect();
    // By `seq`, not by `ItemId` — see `ItemState::seq`.
    items.sort_by_key(|item| item.seq);

    let mut messages: Vec<Message> = Vec::new();
    let mut repairs = Vec::new();
    // Calls this history is still waiting on. A `Vec` and not an `Option`:
    // one generation can dispatch several calls in parallel and the crash
    // that tore one of them can have torn all of them.
    let mut unanswered: Vec<String> = Vec::new();

    let mut next = 0;
    while let Some(item) = items.get(next).copied() {
        next += 1;
        match item.kind {
            // Replayed if — and only if — the provider sealed the block with
            // continuation state.
            //
            // The *text* is still never rebuilt, and that half of the
            // original reason is unchanged: promoting reasoning to assistant
            // text would put words in the model's mouth it never published.
            // What changed is that a block carrying state is no longer the
            // same case. The state is what the provider asks to have handed
            // back, opaque and positional, and dropping it is what produces
            // the documented `400` — including for a block whose `thinking`
            // field is empty, which is exactly the item with state and no
            // text.
            //
            // So the two are separate questions and this asks the right one:
            // state present, replay it; state absent, nothing to replay, and
            // the text is not a substitute for it.
            ItemKind::Reasoning => {
                if let Some(state) = item.reasoning_state.clone() {
                    push(
                        &mut messages,
                        Role::Assistant,
                        ContentBlock::Reasoning { state },
                    );
                }
            }
            ItemKind::UserPrompt => {
                push(&mut messages, Role::User, text_block(&item.text));
            }
            ItemKind::Assistant => {
                push(&mut messages, Role::Assistant, text_block(&item.text));
            }
            // Taken a generation at a time, not a call at a time. The model
            // issued every call in one run at once, so all their uses belong
            // to one assistant turn and all their results to the one user
            // turn that answers it. Emitting each call with its own answer in
            // between would describe a conversation in which the model saw
            // the first result before choosing the second — five turns where
            // three happened. See `POTENTIALS.md` P22.
            ItemKind::ToolCall => {
                let run = generation_run(&items[next - 1..]);
                next += run.len() - 1;
                // The prefix that can actually be sent. A call carrying no
                // provider id is unsendable, and so is everything after it: a
                // result may only answer a call that appears earlier, so the
                // calls that follow cannot be re-parented onto anything.
                // Calls torn *before* it are still answered — by their own
                // result below when they completed, and by `finish` when they
                // did not, because their turn is part of the surviving prefix.
                let sendable: Vec<(&ItemState, String)> = run
                    .iter()
                    .map_while(|call| call.call_id.clone().map(|call_id| (*call, call_id)))
                    .collect();
                let truncated = sendable.len() < run.len();
                for (call, call_id) in &sendable {
                    push(
                        &mut messages,
                        Role::Assistant,
                        ContentBlock::ToolUse {
                            call_id: call_id.clone(),
                            name: call.tool_name.clone().unwrap_or_default(),
                            args: call.args.clone(),
                        },
                    );
                }
                for (call, call_id) in sendable {
                    if call.completed {
                        push(
                            &mut messages,
                            Role::User,
                            ContentBlock::ToolResult {
                                call_id,
                                output: call.text.clone(),
                                // `ok` is absent only on a record that says a
                                // call completed without saying how, which is
                                // not a success anyone observed.
                                is_error: !call.ok.unwrap_or(false),
                            },
                        );
                    } else {
                        unanswered.push(call_id);
                    }
                }
                if truncated {
                    repairs.push(Repair::TruncatedUnidentifiedCall);
                    let conversation = finish(messages, unanswered, &mut repairs)?;
                    return Ok((conversation, repairs));
                }
            }
        }
    }

    let conversation = finish(messages, unanswered, &mut repairs)?;
    Ok((conversation, repairs))
}

/// Answer every outstanding call, then check the result.
fn finish(
    mut messages: Vec<Message>,
    unanswered: Vec<String>,
    repairs: &mut Vec<Repair>,
) -> Result<Conversation, HistoryError> {
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
    Conversation::new(messages)
}

/// The leading run of tool calls the log attributes to one generation.
///
/// `items[0]` is a tool call; the run is every call immediately after it that
/// the log says came out of the same [`AttemptRef`]. The full ref and not the
/// generation alone, because a retry deliberately reuses the generation id —
/// grouping by generation would put a call in the same turn as the attempt it
/// replaced.
///
/// A call the log does not attribute is a run of one. `None` means "this log
/// does not say", not "this call stood alone", and grouping on absence would
/// fabricate parallelism out of a log that never claimed any. Adjacency alone
/// cannot stand in either: sequential and parallel dispatch reduce to the
/// identical item sequence, so neighbouring calls are not evidence of
/// anything.
fn generation_run<'a, 'b>(items: &'b [&'a ItemState]) -> &'b [&'a ItemState] {
    let Some(generation) = items[0].generation else {
        return &items[..1];
    };
    let len = items
        .iter()
        .take_while(|item| item.kind == ItemKind::ToolCall && item.generation == Some(generation))
        .count();
    &items[..len]
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
