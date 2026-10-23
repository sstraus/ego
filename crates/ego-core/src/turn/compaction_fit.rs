//! Bound the recovery request itself, independently of the source occupancy.

use crate::compact::CompactionError;
use crate::generator::types::{ContentBlock, Conversation, GenRequest, Message, Role};

// Text tokenizers can fall back to individual UTF-8 bytes. Counting serialized
// bytes is deliberately conservative, not a claim about measured occupancy.
// Leave additional room for the provider's message framing and special tokens.
const FRAMING_RESERVE: u64 = 4_096;
const MIN_EXCERPT_BYTES: usize = 256;
const OMITTED: &str = "\n[... source content omitted to fit compaction ...]\n";

pub(super) fn fit(
    request: &GenRequest,
    window: Option<u64>,
) -> Result<GenRequest, CompactionError> {
    let Some(window) = window else {
        return Ok(request.clone());
    };
    let budget = window
        .checked_sub(request.max_output_tokens.unwrap_or(0))
        .and_then(|remaining| remaining.checked_sub(FRAMING_RESERVE))
        .ok_or_else(|| unfit(window))?;
    // Encoded media bytes are not an upper bound on image/audio tokens.
    let has_media = request.messages.messages().iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Image { .. } | ContentBlock::Audio { .. }
            )
        })
    });
    if !has_media && size(request)? <= budget {
        return Ok(request.clone());
    }

    // An oversized request must not retain opaque reasoning, media, tool
    // schemas or call/result relations after dropping history. Render source
    // data as a text transcript instead; it is never executed. The durable
    // source and ordinary in-window cache-preserving path remain unchanged.
    let mut fitted = request.clone();
    fitted.cache = None;
    fitted.tools.clear();
    fitted.web_search = None;
    let mut transcript = Vec::new();
    let mut tool_outputs = Vec::new();
    let messages = request.messages.messages();
    let (instruction, history) = messages.split_last().ok_or_else(|| unfit(window))?;
    for message in history {
        for block in &message.content {
            let (text, tool_output) = match block {
                ContentBlock::Text { text } => (text.clone(), false),
                ContentBlock::ToolUse { name, args, .. } => {
                    (format!("Tool call {name}: {args}"), false)
                }
                ContentBlock::ToolResult { output, .. } => (output.clone(), true),
                ContentBlock::Reasoning { .. } | ContentBlock::ProviderContent { .. } => {
                    ("[opaque provider state omitted]".into(), false)
                }
                ContentBlock::Image { .. } | ContentBlock::Audio { .. } => {
                    ("[source media omitted]".into(), false)
                }
            };
            transcript.push(Message::text(
                Role::User,
                format!("Source conversation data ({:?}):\n{text}", message.role),
            ));
            tool_outputs.push(tool_output);
        }
    }
    // Always retain the complete final summarizer instruction, including the
    // correction prompt on the one permitted grammar-repair attempt.
    loop {
        let mut messages = transcript.clone();
        messages.push(instruction.clone());
        fitted.messages = Conversation::new(messages).map_err(CompactionError::History)?;
        if size(&fitted)? <= budget {
            return Ok(fitted);
        }
        // Largest tool output first (oldest on ties), then the largest text.
        // Keep both ends of an oversized first prompt: the initial objective
        // and the latest instructions are often in the same durable item.
        let candidate = transcript
            .iter()
            .enumerate()
            .filter_map(|(index, message)| match &message.content[0] {
                ContentBlock::Text { text } if text.len() > MIN_EXCERPT_BYTES => {
                    Some((index, tool_outputs[index], text.len()))
                }
                _ => None,
            })
            .max_by_key(|(index, tool, bytes)| (*tool, *bytes, std::cmp::Reverse(*index)));
        if let Some((index, _, bytes)) = candidate {
            let ContentBlock::Text { text } = &mut transcript[index].content[0] else {
                unreachable!("transcript contains only text")
            };
            let keep = (bytes / 2).max(MIN_EXCERPT_BYTES) - OMITTED.len();
            let head = text.floor_char_boundary(keep / 2);
            let tail = text.ceil_char_boundary(text.len() - keep / 2);
            *text = format!("{}{OMITTED}{}", &text[..head], &text[tail..]);
        } else if !transcript.is_empty() {
            transcript.remove(0);
            tool_outputs.remove(0);
        } else {
            return Err(unfit(window));
        }
    }
}

fn size(request: &GenRequest) -> Result<u64, CompactionError> {
    serde_json::to_vec(request)
        .map(|bytes| bytes.len() as u64)
        .map_err(|error| CompactionError::Internal(error.to_string()))
}

fn unfit(window: u64) -> CompactionError {
    CompactionError::Internal(format!(
        "The compaction instruction and output reserve cannot fit the {window}-token model window; select a model with a larger window."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compact::COMPACTION_PROMPT;
    use crate::generator::types::ThinkingLevel;

    fn request(messages: Vec<Message>) -> GenRequest {
        GenRequest {
            web_search: None,
            cache: None,
            model: "summarizer".into(),
            messages: Conversation::new(messages).unwrap(),
            tools: Vec::new(),
            thinking: ThinkingLevel::Low,
            max_output_tokens: Some(4_096),
            parallel_tool_calls: false,
        }
    }

    // Catches: dropping a huge tool result leaves dangling calls or destroys
    // recent instructions, and mutates the durable-source request in place.
    #[test]
    fn oversized_tool_output_is_excerpted_without_dangling_calls_or_source_mutation() {
        let source = request(vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    call_id: "call-1".into(),
                    name: "read".into(),
                    args: "{}".into(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: "call-1".into(),
                    output: "界".repeat(100_000),
                    is_error: false,
                }],
            },
            Message::text(Role::User, "latest objective"),
            Message::text(Role::User, COMPACTION_PROMPT),
        ]);
        let original = source.clone();
        let fitted = fit(&source, Some(32_000)).unwrap();
        assert_eq!(source, original);
        assert!(size(&fitted).unwrap() + 4_096 + FRAMING_RESERVE <= 32_000);
        let transcript = serde_json::to_string(&fitted.messages).unwrap();
        assert!(transcript.contains("latest objective"));
        assert!(transcript.contains("source content omitted"));
        assert!(
            fitted
                .messages
                .messages()
                .iter()
                .flat_map(|m| &m.content)
                .all(|block| matches!(block, ContentBlock::Text { .. }))
        );
    }

    // Catches: fitting only the first summarizer attempt lets a large invalid
    // checkpoint overflow the correction request or truncates its instruction.
    #[test]
    fn oversized_checkpoint_correction_keeps_the_complete_final_instruction() {
        let correction = "Return the checkpoint headings in the required order.";
        let source = request(vec![
            Message::text(Role::User, COMPACTION_PROMPT),
            Message::text(Role::Assistant, "invalid checkpoint".repeat(50_000)),
            Message::text(Role::User, correction),
        ]);
        let fitted = fit(&source, Some(32_000)).unwrap();
        assert_eq!(
            fitted.messages.messages().last(),
            source.messages.messages().last()
        );
        assert!(size(&fitted).unwrap() + 4_096 + FRAMING_RESERVE <= 32_000);
    }

    // Catches: zero/tiny windows underflow the budget and admit an unfit request.
    #[test]
    fn impossible_windows_refuse_instead_of_underflowing_or_sending() {
        let source = request(vec![Message::text(Role::User, COMPACTION_PROMPT)]);
        for window in [0, 4_096, 8_192, 8_193] {
            assert!(fit(&source, Some(window)).is_err(), "window={window}");
        }
    }

    // Catches: conservative fitting rewrites already-fitting live history and
    // loses its cache lineage even when no reduction is needed.
    #[test]
    fn fitting_or_unknown_windows_leave_the_original_request_unchanged() {
        let source = request(vec![Message::text(Role::User, COMPACTION_PROMPT)]);
        assert_eq!(fit(&source, Some(32_000)).unwrap(), source);
        assert_eq!(fit(&source, None).unwrap(), source);
    }
}
