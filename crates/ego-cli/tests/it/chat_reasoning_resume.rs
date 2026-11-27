use ego_ai::wire::openai_completions::{
    OPENROUTER_REASONING_DIALECT, StreamDecoder, openrouter_request_body,
};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, SessionId, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::history::conversation_from;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{GenRequest, ThinkingLevel, ToolDescriptor};
use ego_core::permit::ToolResult;
use ego_core::store::session_store::SessionStore;
use ego_tools::clock::SystemClock;
use ego_tools::store::{JsonlStore, OsIo};

const RECORDED_TOOL_CALL: &str = include_str!(
    "../../../ego-ai/tests/goldens/openrouter/deepseek-reasoning-tool-call-2026-11-14.sse"
);
const CONTINUATION: &str = include_str!(
    "../../../ego-ai/tests/goldens/openrouter/deepseek-reasoning-continuation.request.json"
);

// Catches: filesystem session reopen losing exact reasoning or its placement
// before the recorded call and its tool result in the continuation request.
#[test]
fn recorded_chat_tool_continuation_survives_filesystem_reopen() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().canonicalize().unwrap();
    let new_store = || {
        JsonlStore::new(
            root.path().join("sessions"),
            workspace.clone(),
            Box::new(OsIo),
        )
    };
    let session = SessionId::new();
    {
        let store = new_store();
        let turn = TurnId::new();
        let mut seq = Seq::ZERO;
        let mut append = |item, kind| {
            store
                .append(
                    &session,
                    &DomainEvent {
                        env: Envelope {
                            seq: seq.next(),
                            id: EventId::new(),
                            turn,
                            item: Some(item),
                            parent: None,
                            ts: Timestamp::from_millis(0),
                            producer: Producer::Main,
                        },
                        kind,
                    },
                )
                .unwrap();
        };
        append(
            ItemId::new(),
            EventKind::UserPrompt {
                text:
                    "Call get_weather for Rome first. After you get the result, call get_weather \
                       for Paris. Then compare in one sentence."
                        .into(),
                images: Vec::new(),
                audio: Vec::new(),
                queued_prompt: None,
            },
        );

        let mut decoder =
            StreamDecoder::default().replaying_reasoning(OPENROUTER_REASONING_DIALECT);
        let mut events = decoder.push(RECORDED_TOOL_CALL.as_bytes());
        events.extend(decoder.finish(None));
        let reasoning_item = ItemId::new();
        for event in events {
            match event {
                AiEvent::ReasoningDelta { text } => {
                    append(reasoning_item, EventKind::ReasoningDelta { text });
                }
                AiEvent::ReasoningState(state) => {
                    append(reasoning_item, EventKind::ReasoningState { state });
                }
                AiEvent::ToolCall {
                    call_id,
                    name,
                    args,
                } => {
                    let item = ItemId::new();
                    append(
                        item,
                        EventKind::ToolCallStarted {
                            name,
                            args,
                            call_id: Some(call_id),
                        },
                    );
                    append(
                        item,
                        EventKind::ToolCallCompleted {
                            ok: true,
                            result: ToolResult::builtin(r#"{"temp_c": 21}"#),
                        },
                    );
                }
                _ => {}
            }
        }
    }

    let store = new_store();
    let opened = ego_cli::session::open(&store, &session, &SystemClock).unwrap();
    let (messages, repairs) = conversation_from(&opened.state).unwrap();
    assert!(repairs.is_empty(), "the completed call needs no repair");
    let request = GenRequest {
        web_search: None,
        cache: None,
        model: "deepseek/deepseek-v4.1-flash-1017".into(),
        messages,
        tools: vec![ToolDescriptor {
            name: "get_weather".into(),
            description: "Weather for a city".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        }],
        thinking: ThinkingLevel::High,
        max_output_tokens: Some(1500),
        parallel_tool_calls: false,
    };
    let actual: serde_json::Value =
        serde_json::from_slice(&openrouter_request_body(&request).unwrap()).unwrap();
    let expected: serde_json::Value = serde_json::from_str(CONTINUATION).unwrap();
    assert_eq!(actual, expected);
}
