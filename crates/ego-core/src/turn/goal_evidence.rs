//! The session evidence a goal checker reads, as text.
//!
//! Checker input is serialized JSON, not a multimodal request. Each image
//! becomes a bounded descriptor built from its durable metadata: the checker
//! cannot see pictures, so no image blob is read, decoded or hashed for it.
//! `Audio` derives `Serialize` through its plain byte buffer, so each hydrated
//! audio clip becomes a bounded descriptor too. Opaque reasoning is transport
//! continuation state, not readable evidence, so it is omitted. Ordinary
//! storage and main-model requests are unchanged: only this checker projection
//! removes these payloads.

use serde_json::{Value, json};

use crate::generator::history::ImageAnchor;
use crate::generator::types::{ContentBlock, Conversation, ImageRef};

/// The session as the checker reads it: `history` from
/// [`conversation_with_image_anchors`](crate::generator::history::conversation_with_image_anchors),
/// which holds no image block, with an image descriptor at every anchor and
/// every audio clip replaced by a descriptor. The reference names the session
/// blob in its `SHA256:BYTES` spelling; nothing reads it back.
///
/// A descriptor proves nothing about what the picture shows or the clip says,
/// and an image descriptor does not check that its blob is still present, so
/// it says `available_to_checker: false`. The checker contract treats a
/// criterion that only the media could establish as unverified.
pub(super) fn session(history: &Conversation, images: &[ImageAnchor]) -> Value {
    let mut images = images.iter().peekable();
    Value::Array(
        history
            .messages()
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                let mut content = Vec::new();
                for position in 0..=message.content.len() {
                    while let Some(anchor) =
                        images.next_if(|anchor| anchor.message == index && anchor.block == position)
                    {
                        content.push(descriptor(&anchor.image));
                    }
                    content.extend(message.content.get(position).and_then(block));
                }
                (!content.is_empty()).then(|| {
                    json!({
                        "role": message.role,
                        "content": content,
                    })
                })
            })
            .collect(),
    )
}

/// Whether the checker input holds an audio clip it cannot hear.
pub(super) fn has_audio(history: &Conversation) -> bool {
    history.messages().iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Audio { .. }))
    })
}

fn descriptor(image: &ImageRef) -> Value {
    json!({
        "image_evidence": {
            "reference": image.reference().to_string(),
            "media_type": image.media_type(),
            "width": image.width(),
            "height": image.height(),
            "available_to_checker": false,
        }
    })
}

fn block(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Reasoning { .. } => None,
        ContentBlock::Audio { audio } => Some(json!({
            "audio_evidence": {
                "reference": format!(
                    "{}:{}",
                    crate::digest::sha256_hex(audio.bytes()),
                    audio.bytes().len()
                ),
                "media_type": audio.media_type(),
                "available_to_checker": false,
            }
        })),
        other => Some(
            serde_json::to_value(other)
                .unwrap_or_else(|error| json!({ "unserializable_block": error.to_string() })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::event::domain::{DomainEvent, Envelope, EventKind};
    use crate::event::id::{EventId, ItemId, Producer, Seq, TurnId};
    use crate::event::time::Timestamp;
    use crate::generator::history::{
        conversation_from_with_blob_reader, conversation_with_image_anchors,
    };
    use crate::generator::types::{HistoryError, ImageMediaType};
    use crate::permit::{ToolContentBlock, ToolResult};
    use crate::state::SessionState;
    use crate::store::blob::{BlobError, BlobId, BlobRef};

    // A valid 1x1 PNG.
    const PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4,
        0, 0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 100, 248, 15, 0, 1, 5,
        1, 1, 39, 24, 227, 102, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// The projection this story replaces, from hydrated bytes: the oracle.
    fn hydrated_projection(history: &Conversation) -> Value {
        Value::Array(
            history
                .messages()
                .iter()
                .map(|message| {
                    let content: Vec<_> = message
                        .content
                        .iter()
                        .map(|block| match block {
                            ContentBlock::Image { image } => json!({"image_evidence": {
                                "reference": format!(
                                    "{}:{}",
                                    crate::digest::sha256_hex(image.bytes()),
                                    image.bytes().len()
                                ),
                                "media_type": image.media_type(),
                                "width": image.width(),
                                "height": image.height(),
                                "available_to_checker": false,
                            }}),
                            other => serde_json::to_value(other).unwrap(),
                        })
                        .collect();
                    json!({ "role": message.role, "content": content })
                })
                .collect(),
        )
    }

    // Catches: the goal checker still reads (or needs) image blobs, so a missing blob
    // stops the check; or a metadata descriptor lands at another position than the
    // image it replaces (prompt offsets, a legacy leading image, the frame rule that
    // opens a new message, or tool-result images after their paired result).
    #[test]
    fn checker_evidence_names_images_from_metadata_in_request_order() {
        let reference = BlobRef::new(
            BlobId::parse(crate::digest::sha256_hex(PNG)).unwrap(),
            PNG.len() as u64,
        );
        let image = ImageRef::new(reference.clone(), ImageMediaType::Png, 1, 1).unwrap();
        let call = ItemId::new();
        let mut state = SessionState::default();
        let mut seq = Seq::ZERO;
        for (item, kind) in [
            (
                ItemId::new(),
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: "before|after".into(),
                    // No offset is the leading position of records written before offsets.
                    images: vec![
                        image.clone(),
                        image.clone().with_text_offset(6),
                        image.clone().with_text_offset(6),
                    ],
                    queued_prompt: None,
                },
            ),
            (
                ItemId::new(),
                EventKind::HostContextAppended {
                    text: "Current context data. host facts".into(),
                },
            ),
            (
                ItemId::new(),
                EventKind::UserPrompt {
                    audio: Vec::new(),
                    text: String::new(),
                    images: vec![image.clone()],
                    queued_prompt: None,
                },
            ),
            (
                call,
                EventKind::ToolCallStarted {
                    name: "read".into(),
                    args: "{}".into(),
                    call_id: Some("read-1".into()),
                },
            ),
            (
                call,
                EventKind::ToolCallCompleted {
                    ok: true,
                    result: ToolResult::new(vec![ToolContentBlock::builtin("receipt")])
                        .with_images(vec![image.clone(), image.clone()]),
                },
            ),
        ] {
            state.apply(&DomainEvent {
                env: Envelope {
                    seq: seq.next(),
                    id: EventId::new(),
                    turn: TurnId::new(),
                    item: Some(item),
                    parent: None,
                    ts: Timestamp::from_millis(0),
                    producer: Producer::Main,
                },
                kind,
            });
        }
        let present = |wanted: &BlobRef| -> Result<Vec<u8>, BlobError> {
            assert_eq!(wanted, &reference);
            Ok(PNG.to_vec())
        };
        let (hydrated, _) = conversation_from_with_blob_reader(&state, &present).unwrap();
        let expected = hydrated_projection(&hydrated).to_string();

        let reads = Cell::new(0);
        let missing = |wanted: &BlobRef| -> Result<Vec<u8>, BlobError> {
            reads.set(reads.get() + 1);
            Err(BlobError::NotFound {
                reference: wanted.id.clone(),
            })
        };
        assert!(matches!(
            conversation_from_with_blob_reader(&state, &missing),
            Err(HistoryError::ImageUnavailable { .. })
        ));
        reads.set(0);
        let (history, images) = conversation_with_image_anchors(&state, &missing).unwrap();
        assert_eq!(reads.get(), 0, "the checker projection read an image blob");
        assert_eq!(images.len(), 6);
        assert_eq!(session(&history, &images).to_string(), expected);
    }
}
