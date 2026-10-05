//! The provider-neutral conversation: what a valid tool-use history is, and
//! what happens to one that a crash left half-finished.
//!
//! See `ARCHITECTURE.md`. The invariants tested
//! here are the ones *every* wire format shares — a result answers an earlier
//! call, exactly once, and no call is left hanging. Rules only one provider
//! has (Anthropic's grouping and ordering; chat-completions' encodability)
//! stay in the wire formats and are not tested here, because the two
//! providers disagree and a core-level answer would be wrong for one of them.

use ego_core::event::domain::{DomainEvent, Envelope, EventKind, GenerationRole};
use ego_core::event::id::{EventId, GenerationId, ItemId, Producer, Seq, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::attempt::AttemptRef;
use ego_core::generator::history::{Repair, conversation_from};
use ego_core::generator::types::{
    ContentBlock, Conversation, HistoryError, Message, ReasoningState, Role,
};
use ego_core::permit::ToolResult;
use ego_core::state::SessionState;

mod conversation {
    use super::*;

    fn text(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    fn call(call_id: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            call_id: call_id.to_string(),
            name: "read_file".to_string(),
            args: r#"{"path":"a.txt"}"#.to_string(),
        }
    }

    fn result(call_id: &str) -> ContentBlock {
        ContentBlock::ToolResult {
            call_id: call_id.to_string(),
            output: "contents".to_string(),
            is_error: false,
        }
    }

    fn assistant(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content: blocks,
        }
    }

    fn user(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::User,
            content: blocks,
        }
    }

    #[test]
    fn a_complete_tool_round_trip_is_a_conversation() {
        let history = vec![
            text(Role::User, "what is in a.txt?"),
            assistant(vec![
                ContentBlock::Text {
                    text: "Let me look.".to_string(),
                },
                call("call_a"),
            ]),
            user(vec![result("call_a")]),
        ];
        let conversation = Conversation::new(history.clone()).expect("a valid history");
        assert_eq!(conversation.messages(), history.as_slice());
    }

    /// Several calls in one assistant turn, several results in one user turn —
    /// the shape Anthropic requires (`.../tool-use/parallel-tool-use`: one
    /// result per call, grouped into the next user message). The old
    /// `Message::tool_call_id` could not express it at all: one optional id
    /// per message cannot answer two calls.
    #[test]
    fn parallel_calls_are_answered_in_one_turn() {
        let history = vec![
            text(Role::User, "read both"),
            assistant(vec![call("call_a"), call("call_b")]),
            user(vec![result("call_a"), result("call_b")]),
        ];
        Conversation::new(history).expect("two calls answered by two results");
    }

    #[test]
    fn a_result_that_answers_nothing_is_refused() {
        let err = Conversation::new(vec![
            text(Role::User, "hello"),
            user(vec![result("call_ghost")]),
        ])
        .expect_err("a result naming no call is not a history");
        assert_eq!(
            err,
            HistoryError::ResultWithoutCall {
                call_id: "call_ghost".to_string()
            }
        );
    }

    /// Order is part of the invariant, not just membership: a result may only
    /// answer a call that has already been made. A history that answers first
    /// and calls afterwards is not a conversation that happened.
    #[test]
    fn a_result_before_its_call_is_refused() {
        let err = Conversation::new(vec![
            user(vec![result("call_a")]),
            assistant(vec![call("call_a")]),
        ])
        .expect_err("a result cannot precede the call it answers");
        assert_eq!(
            err,
            HistoryError::ResultWithoutCall {
                call_id: "call_a".to_string()
            }
        );
    }

    #[test]
    fn answering_one_call_twice_is_refused() {
        let err = Conversation::new(vec![
            assistant(vec![call("call_a")]),
            user(vec![result("call_a")]),
            user(vec![result("call_a")]),
        ])
        .expect_err("one call has one result");
        assert_eq!(
            err,
            HistoryError::CallAnsweredTwice {
                call_id: "call_a".to_string()
            }
        );
    }

    /// The id is the key the whole invariant is built on, so two calls sharing
    /// one is not a lesser problem than a duplicate answer: it makes "which
    /// call did this result answer" unanswerable.
    #[test]
    fn two_calls_sharing_an_id_are_refused() {
        let err = Conversation::new(vec![assistant(vec![call("call_a"), call("call_a")])])
            .expect_err("a provider issues an id once");
        assert_eq!(
            err,
            HistoryError::DuplicateCallId {
                call_id: "call_a".to_string()
            }
        );
    }

    /// The case that blocks the turn loop: a provider that receives a call it
    /// never sees answered rejects the request. Refusing here is what forces
    /// the caller to decide, instead of finding out from a 400.
    #[test]
    fn a_call_nobody_answered_is_refused() {
        let err = Conversation::new(vec![
            text(Role::User, "read it"),
            assistant(vec![call("call_a")]),
        ])
        .expect_err("an unanswered call cannot be sent");
        assert_eq!(
            err,
            HistoryError::CallUnanswered {
                call_id: "call_a".to_string()
            }
        );
    }

    /// Universal in substance, provider-specific only in spelling: OpenAI
    /// gives a result its own `tool` role and Anthropic puts it in a `user`
    /// turn, but on neither wire does anyone but the model ask for a tool, or
    /// the model answer itself. Checking it here keeps an identical check out
    /// of every adapter.
    #[test]
    fn only_the_assistant_calls_and_never_answers_itself() {
        let called_by_the_user = Conversation::new(vec![user(vec![call("call_a")])])
            .expect_err("a user turn does not call tools");
        assert_eq!(
            called_by_the_user,
            HistoryError::CallFromNonAssistant {
                call_id: "call_a".to_string()
            }
        );

        let answered_by_itself =
            Conversation::new(vec![assistant(vec![call("call_a"), result("call_a")])])
                .expect_err("the assistant does not answer its own call");
        assert_eq!(
            answered_by_itself,
            HistoryError::ResultFromAssistant {
                call_id: "call_a".to_string()
            }
        );
    }

    /// An error result still answers the call. Anthropic says so outright —
    /// an unexecuted call must be answered "with a tool_result marked with
    /// `is_error` set to true" — so `is_error` must not read as "no answer".
    #[test]
    fn an_error_result_is_still_an_answer() {
        Conversation::new(vec![
            assistant(vec![call("call_a")]),
            user(vec![ContentBlock::ToolResult {
                call_id: "call_a".to_string(),
                output: "no such file".to_string(),
                is_error: true,
            }]),
        ])
        .expect("a failure is an answer");
    }

    /// A `Conversation` that arrives as bytes has to be checked like one that
    /// arrives as a `Vec`: a deserialiser that skipped the constructor would
    /// be a hole straight through the invariant.
    #[test]
    fn deserialising_a_conversation_checks_it_too() {
        let valid = serde_json::to_string(
            &Conversation::new(vec![
                assistant(vec![call("call_a")]),
                user(vec![result("call_a")]),
            ])
            .expect("a valid history"),
        )
        .expect("a conversation serialises");
        serde_json::from_str::<Conversation>(&valid).expect("a valid history round-trips");

        let broken = serde_json::to_string(&vec![user(vec![result("call_ghost")])])
            .expect("messages serialise");
        serde_json::from_str::<Conversation>(&broken)
            .expect_err("an invalid history must not decode into a Conversation");
    }
}

/// Rebuilding a history from the reduced log, including the two shapes a
/// crash leaves behind.
mod rebuild {
    use super::*;

    struct Log {
        state: SessionState,
        seq: Seq,
        turn: TurnId,
    }

    impl Log {
        fn new() -> Self {
            Self {
                state: SessionState::default(),
                seq: Seq::ZERO,
                turn: TurnId::new(),
            }
        }

        fn push(&mut self, item: Option<ItemId>, kind: EventKind) -> &mut Self {
            self.append(item, None, kind);
            self
        }

        /// Append one event and hand back the id a later one can name.
        fn append(
            &mut self,
            item: Option<ItemId>,
            parent: Option<EventId>,
            kind: EventKind,
        ) -> EventId {
            let id = EventId::new();
            self.state.apply(&DomainEvent {
                env: Envelope {
                    seq: self.seq.next(),
                    id,
                    turn: self.turn,
                    item,
                    parent,
                    ts: Timestamp::from_millis(1_000),
                    producer: Producer::Main,
                },
                kind,
            });
            id
        }

        fn item(&mut self, kind: EventKind) -> ItemId {
            let item = ItemId::new();
            self.push(Some(item), kind);
            item
        }

        /// Open a generation, returning the event id its items name as parent.
        fn generation(&mut self) -> EventId {
            self.append(
                None,
                None,
                EventKind::GenerationStarted {
                    attempt: AttemptRef::first(GenerationId::new()),
                    role: GenerationRole::Main,
                    snapshot: None,
                },
            )
        }

        /// An item the log says `generation` produced.
        fn item_under(&mut self, generation: EventId, kind: EventKind) -> ItemId {
            let item = ItemId::new();
            self.append(Some(item), Some(generation), kind);
            item
        }
    }

    fn prompt(text: &str) -> EventKind {
        EventKind::UserPrompt {
            text: text.to_string(),
            images: Vec::new(),
            queued_prompt: None,
        }
    }

    fn started(call_id: Option<&str>) -> EventKind {
        EventKind::ToolCallStarted {
            name: "read_file".to_string(),
            args: r#"{"path":"a.txt"}"#.to_string(),
            call_id: call_id.map(str::to_string),
        }
    }

    #[test]
    fn a_finished_turn_rebuilds_to_the_conversation_that_produced_it() {
        let mut log = Log::new();
        log.item(prompt("what is in a.txt?"));
        log.item(EventKind::AssistantMessage {
            text: "Let me look.".to_string(),
        });
        let call = log.item(started(Some("call_a")));
        log.push(
            Some(call),
            EventKind::ToolCallCompleted {
                ok: true,
                result: ToolResult::builtin("contents"),
            },
        );

        let (conversation, repairs) =
            conversation_from(&log.state).expect("a repairable history rebuilds");
        assert!(repairs.is_empty(), "nothing was torn");
        assert_eq!(
            conversation.messages(),
            &[
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "what is in a.txt?".to_string()
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: "Let me look.".to_string()
                        },
                        ContentBlock::ToolUse {
                            call_id: "call_a".to_string(),
                            name: "read_file".to_string(),
                            args: r#"{"path":"a.txt"}"#.to_string(),
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        call_id: "call_a".to_string(),
                        output: "contents".to_string(),
                        is_error: false,
                    }],
                },
            ]
        );
    }

    /// The reducer used to write a call's `args` into the same field it later
    /// wrote the output into, so a *completed* call could not say what it was
    /// called with. This is the assertion that would have caught it: the
    /// arguments above survive the completion.
    #[test]
    fn a_completed_call_still_knows_what_it_was_called_with() {
        let mut log = Log::new();
        let call = log.item(started(Some("call_a")));
        log.push(
            Some(call),
            EventKind::ToolCallDelta {
                text: "cont".to_string(),
            },
        );
        log.push(
            Some(call),
            EventKind::ToolCallCompleted {
                ok: true,
                result: ToolResult::builtin("contents"),
            },
        );

        let item = &log.state.items[&call];
        assert_eq!(item.args, r#"{"path":"a.txt"}"#);
        assert!(
            item.text.is_empty(),
            "completed tool deltas are not the result"
        );
        assert_eq!(
            item.tool_result.as_ref().map(ToolResult::model_text),
            Some("contents".to_string())
        );
    }

    /// The Step 12 case: killed between dispatching a call and recording its
    /// result. The provider contract says an unexecuted call must still be
    /// answered, so the rebuild answers it rather than deleting the turn that
    /// made it.
    #[test]
    fn a_torn_call_is_answered_rather_than_erased() {
        let mut log = Log::new();
        log.item(prompt("read it"));
        log.item(started(Some("call_a")));

        let (conversation, repairs) =
            conversation_from(&log.state).expect("a repairable history rebuilds");
        assert_eq!(
            repairs,
            vec![Repair::AnsweredTornCall {
                call_id: "call_a".to_string()
            }]
        );
        let last = conversation
            .messages()
            .last()
            .expect("the synthesized answer");
        assert_eq!(last.role, Role::User);
        match &last.content[..] {
            [
                ContentBlock::ToolResult {
                    call_id, is_error, ..
                },
            ] => {
                assert_eq!(call_id, "call_a");
                assert!(is_error, "an unfinished call did not succeed");
            }
            other => panic!("expected one error result, got {other:?}"),
        }
        assert_eq!(
            conversation.messages().len(),
            3,
            "the prompt and the assistant turn that made the call both survive"
        );
    }

    /// `POTENTIALS.md` P21, answered at the only cost worth paying.
    ///
    /// The synthesized answer is the harness talking to a model that is about
    /// to decide whether to repeat a call nobody knows the outcome of, and
    /// with `write` in the registry that call may have already landed bytes.
    /// Saying only "it did not complete" invites the repeat, and for a plain
    /// replace the target is now the old contents, the new contents, or a
    /// truncated file — three states the model cannot distinguish without
    /// looking.
    ///
    /// So the text says to look. That is the whole answer: **no
    /// `safe_to_repeat` on `ToolDescriptor`, no per-dispatch delivery
    /// certainty, no automatic replay and no automatic suppression** — the
    /// generic machinery P21 sketches is built when something needs to make a
    /// decision without a model in the loop, and here there is one. Pi
    /// synthesizes "No result provided" and never replays; Hermes strips
    /// dangling call tails on resume and forbids re-execution. Both leave the
    /// judgement where this does.
    ///
    /// Pinned because it is a behavioural contract and not a string: a message
    /// that stops telling the model to check is a message that has silently
    /// changed what a resumed session does after a write.
    #[test]
    fn the_answer_to_a_torn_call_tells_the_model_to_look_before_repeating_it() {
        let mut log = Log::new();
        log.item(prompt("write it"));
        log.item(started(Some("call_a")));

        let (conversation, _) = conversation_from(&log.state).unwrap();
        let [ContentBlock::ToolResult { output, .. }] =
            &conversation.messages().last().unwrap().content[..]
        else {
            panic!("the synthesized answer is one tool result");
        };

        assert!(
            output.contains("did not complete"),
            "it still has to say what happened: {output}"
        );
        assert!(
            output.contains("may already have"),
            "a torn call may have taken effect, and the model has to be told: {output}"
        );
        assert!(
            output.to_lowercase().contains("check"),
            "and told what to do about it before repeating the call: {output}"
        );
    }

    /// A record written before `call_id` was persisted. The id is the
    /// provider's and unguessable, so this call can never be sent — and,
    /// because a call must appear before its answer, neither can anything
    /// after it. Truncation is the only option here, and the only place it is
    /// the right one.
    #[test]
    fn a_call_with_no_id_truncates_instead_of_being_answered() {
        let mut log = Log::new();
        log.item(prompt("read it"));
        let call = log.item(started(None));
        log.push(
            Some(call),
            EventKind::ToolCallCompleted {
                ok: true,
                result: ToolResult::builtin("contents"),
            },
        );
        log.item(EventKind::AssistantMessage {
            text: "it says hello".to_string(),
        });

        let (conversation, repairs) =
            conversation_from(&log.state).expect("a repairable history rebuilds");
        assert_eq!(repairs, vec![Repair::TruncatedUnidentifiedCall]);
        assert_eq!(
            conversation.messages(),
            &[Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "read it".to_string()
                }],
            }],
            "everything from the unidentifiable call onwards is dropped"
        );
    }

    /// A log that already contains two calls under one provider id — written
    /// before the ingress guard existed, or by something that is not this
    /// build. The rebuild refuses it (ADR §6): §4's repairs complete a history
    /// the harness itself tore, and this is not that. Truncating from the
    /// duplicate would drop the `UserPrompt` of every turn after it on *every*
    /// future rebuild, because the log is never rewritten — a permanent silent
    /// amputation, not a one-time repair.
    #[test]
    fn duplicate_call_ids_are_not_repaired_lossily() {
        let mut log = Log::new();
        log.item(prompt("read it twice"));
        for _ in 0..2 {
            let call = log.item(started(Some("call_a")));
            log.push(
                Some(call),
                EventKind::ToolCallCompleted {
                    ok: true,
                    result: ToolResult::builtin("contents"),
                },
            );
        }

        assert_eq!(
            conversation_from(&log.state),
            Err(HistoryError::DuplicateCallId {
                call_id: "call_a".to_string()
            }),
            "corruption the provider sent is refused, not rebuilt into a reading of it"
        );
    }

    /// `SessionState.items` is a `HashMap`, so the order has to come from a
    /// field. `ItemId` is a UUIDv7 and therefore usually sorts the way the
    /// events arrived, which is exactly what makes ordering by it look
    /// correct until it isn't: here the *older* id is used second, so
    /// sorting by id and sorting by `seq` disagree.
    #[test]
    fn the_rebuild_orders_by_sequence_and_not_by_item_id() {
        let older = ItemId::new();
        let newer = ItemId::new();
        assert!(older < newer, "UUIDv7 sorts by creation");

        let mut log = Log::new();
        log.push(Some(newer), prompt("asked first"));
        log.push(
            Some(older),
            EventKind::AssistantMessage {
                text: "answered second".to_string(),
            },
        );

        let (conversation, _) =
            conversation_from(&log.state).expect("a repairable history rebuilds");
        assert_eq!(
            conversation.messages()[0].content,
            vec![ContentBlock::Text {
                text: "asked first".to_string()
            }],
            "the event that arrived first comes first, whatever its item id"
        );
    }

    /// Reasoning *text* is a transcript of something the model published
    /// under its own rules, and rebuilding it as assistant text would put
    /// words in the model's mouth that it never said as its answer. An item
    /// that carries only text has nothing the provider asked to get back, so
    /// it contributes nothing to the history.
    #[test]
    fn reasoning_is_not_rebuilt_as_assistant_text() {
        let mut log = Log::new();
        log.item(prompt("think about it"));
        log.item(EventKind::ReasoningDelta {
            text: "hmm".to_string(),
        });
        log.item(EventKind::AssistantMessage {
            text: "done".to_string(),
        });

        let (conversation, _) =
            conversation_from(&log.state).expect("a repairable history rebuilds");
        assert_eq!(conversation.messages().len(), 2);
        assert_eq!(
            conversation.messages()[1].content,
            vec![ContentBlock::Text {
                text: "done".to_string()
            }]
        );
    }

    /// The block the provider sealed comes back where it stood. Its position
    /// is the contract: Anthropic documents a `400` for state replayed out of
    /// order, and one for a block dropped from the middle of a turn.
    ///
    /// This item has state and no text at all — exactly the block whose
    /// `thinking` field is empty, which is the case the `400` calls out by
    /// name and the one the old empty arm silently dropped.
    #[test]
    fn a_reasoning_block_with_state_is_replayed_where_it_stood() {
        let mut log = Log::new();
        log.item(prompt("think about it"));
        log.item(EventKind::ReasoningState {
            state: ReasoningState {
                blob: "sealed".to_string(),
                dialect: "anthropic".to_string(),
            },
        });
        log.item(EventKind::AssistantMessage {
            text: "done".to_string(),
        });

        let (conversation, repairs) =
            conversation_from(&log.state).expect("a repairable history rebuilds");
        assert!(repairs.is_empty(), "nothing was torn");
        assert_eq!(
            conversation.messages(),
            &[
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "think about it".to_string()
                    }],
                },
                // One assistant turn, not two: the sealed block and the answer
                // it produced were published together.
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Reasoning {
                            state: ReasoningState {
                                blob: "sealed".to_string(),
                                dialect: "anthropic".to_string(),
                            },
                        },
                        ContentBlock::Text {
                            text: "done".to_string()
                        },
                    ],
                },
            ]
        );
    }

    /// State and text are separate questions about the same item, and the
    /// rebuild answers each one on its own terms: the state goes back because
    /// the provider asked for it, and the text stays out because nobody did.
    #[test]
    fn a_block_with_both_carries_its_state_back_and_leaves_its_text_behind() {
        let mut log = Log::new();
        log.item(prompt("think about it"));
        let reasoning = log.item(EventKind::ReasoningDelta {
            text: "hmm".to_string(),
        });
        log.push(
            Some(reasoning),
            EventKind::ReasoningState {
                state: ReasoningState {
                    blob: "sealed".to_string(),
                    dialect: "anthropic".to_string(),
                },
            },
        );

        let (conversation, _) =
            conversation_from(&log.state).expect("a repairable history rebuilds");
        assert_eq!(
            conversation.messages()[1].content,
            vec![ContentBlock::Reasoning {
                state: ReasoningState {
                    blob: "sealed".to_string(),
                    dialect: "anthropic".to_string(),
                },
            }],
            "the blob, and nothing the model said while producing it"
        );
    }

    /// The shape of a turn is a claim about what the model did, and two calls
    /// issued at once are one decision. Rebuilt one call at a time they become
    /// five turns where three happened — a conversation in which the model saw
    /// the first answer before choosing the second, which is not what it did
    /// and invites a different continuation. See `POTENTIALS.md` P22.
    #[test]
    fn two_calls_from_one_generation_rebuild_as_the_single_turn_they_were() {
        let mut log = Log::new();
        log.item(prompt("read both files"));
        let generation = log.generation();
        let first = log.item_under(generation, started(Some("call_a")));
        let second = log.item_under(generation, started(Some("call_b")));
        for (call, output) in [(first, "a"), (second, "b")] {
            log.push(
                Some(call),
                EventKind::ToolCallCompleted {
                    ok: true,
                    result: ToolResult::builtin(output),
                },
            );
        }

        let (conversation, repairs) =
            conversation_from(&log.state).expect("a repairable history rebuilds");
        assert!(repairs.is_empty(), "nothing was torn");
        assert_eq!(
            shape(&conversation),
            [(Role::User, 1), (Role::Assistant, 2), (Role::User, 2)],
            "both uses belong to one assistant turn and both results to the \
             user turn that answers it"
        );
    }

    /// The converse, and the reason grouping cannot be done by adjacency:
    /// sequential dispatch produces the identical item sequence, so a rebuild
    /// that grouped neighbours would fabricate parallelism exactly as readily
    /// as the old one fabricated sequence.
    #[test]
    fn two_calls_from_two_generations_rebuild_as_the_two_turns_they_were() {
        let mut log = Log::new();
        log.item(prompt("read one, then the other"));
        for (call_id, output) in [("call_a", "a"), ("call_b", "b")] {
            let generation = log.generation();
            let call = log.item_under(generation, started(Some(call_id)));
            log.push(
                Some(call),
                EventKind::ToolCallCompleted {
                    ok: true,
                    result: ToolResult::builtin(output),
                },
            );
        }

        let (conversation, _) =
            conversation_from(&log.state).expect("a repairable history rebuilds");
        assert_eq!(
            shape(&conversation),
            [
                (Role::User, 1),
                (Role::Assistant, 1),
                (Role::User, 1),
                (Role::Assistant, 1),
                (Role::User, 1),
            ],
            "the model did see the first answer before choosing the second"
        );
    }

    /// Truncation cuts at the unidentifiable call, and grouping does not move
    /// the cut. The call beside it in the same generation was sent, answered
    /// and recorded, so it keeps the result it earned — a group is not an
    /// all-or-nothing unit, and answering it with the torn-call text would
    /// tell the model its tool said something it never said.
    #[test]
    fn an_unidentified_call_does_not_take_its_generations_answered_call_with_it() {
        let mut log = Log::new();
        log.item(prompt("read both"));
        let generation = log.generation();
        let answered = log.item_under(generation, started(Some("call_a")));
        log.push(
            Some(answered),
            EventKind::ToolCallCompleted {
                ok: true,
                result: ToolResult::builtin("contents"),
            },
        );
        log.item_under(generation, started(None));

        let (conversation, repairs) =
            conversation_from(&log.state).expect("a repairable history rebuilds");
        assert_eq!(repairs, vec![Repair::TruncatedUnidentifiedCall]);
        assert_eq!(
            shape(&conversation),
            [(Role::User, 1), (Role::Assistant, 1), (Role::User, 1)],
            "the identified call and its answer survive; the other is dropped"
        );
        assert_eq!(
            conversation.messages()[2].content,
            vec![ContentBlock::ToolResult {
                call_id: "call_a".to_string(),
                output: "contents".to_string(),
                is_error: false,
            }],
            "the surviving call keeps the result the tool actually returned"
        );
    }

    /// Every turn's author and how many blocks it carries — the whole of what
    /// grouping changes, and nothing about the blocks themselves.
    fn shape(conversation: &Conversation) -> Vec<(Role, usize)> {
        conversation
            .messages()
            .iter()
            .map(|message| (message.role, message.content.len()))
            .collect()
    }
}
