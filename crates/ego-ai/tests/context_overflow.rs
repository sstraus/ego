//! RED contract for regression case: provider context overflow is a distinct cause.

use ego_ai::wire::anthropic_messages::StreamDecoder as AnthropicStreamDecoder;
use ego_ai::wire::openai_completions::StreamDecoder as OpenAiStreamDecoder;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ErrorKind, StopReason};

fn anthropic_error_events(provider_type: &str, message: &str) -> Vec<AiEvent> {
    let transcript = format!(
        "event: error\n\
         data: {{\"type\":\"error\",\"error\":{{\"type\":\"{provider_type}\",\"message\":\"{message}\"}}}}\n\n"
    );
    let mut decoder = AnthropicStreamDecoder::default();
    let mut events = decoder.push(transcript.as_bytes());
    events.extend(decoder.finish(None));
    events
}

fn anthropic_stop_events(stop_reason: &str) -> Vec<AiEvent> {
    let transcript = format!(
        "event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop_reason}\"}}}}\n\n"
    );
    let mut decoder = AnthropicStreamDecoder::default();
    let mut events = decoder.push(transcript.as_bytes());
    events.extend(decoder.finish(None));
    events
}

fn openai_error_events(code: &str, message: &str) -> Vec<AiEvent> {
    let transcript =
        format!("data: {{\"error\":{{\"code\":\"{code}\",\"message\":\"{message}\"}}}}\n\n");
    let mut decoder = OpenAiStreamDecoder::default();
    let mut events = decoder.push(transcript.as_bytes());
    events.extend(decoder.finish(None));
    events
}

#[test]
fn request_too_large_remains_an_ordinary_invalid_provider_error() {
    let events = anthropic_error_events("request_too_large", "request rejected");
    assert!(events.iter().any(|event| matches!(
        event,
        AiEvent::Error { kind: ErrorKind::Invalid, message }
            if message == "request rejected"
    )));
    assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
}

#[test]
fn exact_anthropic_stop_reason_maps_to_context_overflow() {
    let events = anthropic_stop_events("model_context_window_exceeded");
    assert!(events.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::ContextOverflow,
            ..
        }
    )));
}

#[test]
fn exact_openai_error_code_maps_to_context_overflow() {
    let events = openai_error_events("context_length_exceeded", "too many tokens");
    assert!(events.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::ContextOverflow,
            message
        } if message == "too many tokens"
    )));
}

#[test]
fn generic_invalid_request_and_ordinary_output_length_remain_distinct() {
    let events = anthropic_error_events("invalid_request_error", "bad parameter");
    assert!(events.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::Invalid,
            ..
        }
    )));
    let mut decoder = AnthropicStreamDecoder::default();
    let transcript = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n";
    let mut length = decoder.push(transcript.as_bytes());
    length.extend(decoder.finish(None));
    assert!(
        length
            .iter()
            .any(|event| matches!(event, AiEvent::Stop(StopReason::Length)))
    );
}

#[test]
fn overflow_is_not_a_retryable_transient_error() {
    let events = anthropic_stop_events("model_context_window_exceeded");
    assert!(!events.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::Transient,
            ..
        }
    )));
}

#[test]
fn generic_openai_error_and_message_text_do_not_become_context_overflow() {
    let generic = openai_error_events("invalid_request_error", "context length exceeded");
    assert!(generic.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::Unclassified,
            message
        } if message == "context length exceeded"
    )));
    assert!(!generic.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::ContextOverflow,
            ..
        }
    )));

    let status_like = openai_error_events("", "HTTP 413 request entity too large");
    assert!(status_like.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::Unclassified,
            ..
        }
    )));
    assert!(!status_like.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::ContextOverflow,
            ..
        }
    )));
}

#[test]
fn generic_anthropic_status_and_output_stop_remain_distinct() {
    let generic = anthropic_error_events("invalid_request_error", "HTTP 413");
    assert!(generic.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::Invalid,
            ..
        }
    )));
    assert!(!generic.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::ContextOverflow,
            ..
        }
    )));
    let ordinary = anthropic_stop_events("max_tokens");
    assert!(
        ordinary
            .iter()
            .any(|event| matches!(event, AiEvent::Stop(StopReason::Length)))
    );
}
