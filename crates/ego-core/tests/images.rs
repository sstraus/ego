//! RED contract for immutable images, capability schema v4, and durable image metadata.
//!
//! The image value is deliberately constructed through its public serde shape in
//! this module. That keeps the contract independent of constructor convenience
//! methods while still exercising validation at the domain boundary.

use ego_core::event::domain::EventKind;
use ego_core::event::id::QueuedPromptId;
use ego_core::generator::capabilities::{CapabilityGap, ModelCapabilities};
use ego_core::generator::types::{Conversation, GenRequest};
use serde_json::{Value, json};

const PNG_1X1: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4, 0,
    0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 100, 248, 15, 0, 1, 5, 1, 1,
    39, 24, 227, 102, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];
const SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const GIF_1X1: &[u8] = &[
    71, 73, 70, 56, 57, 97, 1, 0, 1, 0, 128, 0, 0, 255, 255, 255, 0, 0, 0, 33, 249, 4, 1, 0, 0, 0,
    0, 44, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 68, 1, 0, 59,
];
const JPEG_1X1: &[u8] = &[
    255, 216, 255, 224, 0, 16, 74, 70, 73, 70, 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, 255, 219, 0, 67, 0, 8,
    6, 6, 7, 6, 5, 8, 7, 7, 7, 9, 9, 8, 10, 12, 20, 13, 12, 11, 11, 12, 25, 18, 19, 15, 20, 29, 26,
    31, 30, 29, 26, 28, 28, 32, 36, 46, 39, 32, 34, 44, 35, 28, 28, 40, 55, 41, 44, 48, 49, 52, 52,
    52, 31, 39, 57, 61, 56, 50, 60, 46, 51, 52, 50, 255, 192, 0, 17, 8, 0, 1, 0, 1, 3, 1, 34, 0, 2,
    17, 1, 3, 17, 1, 255, 196, 0, 20, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255,
    196, 0, 20, 16, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 218, 0, 12, 3, 1, 0,
    2, 17, 3, 17, 0, 63, 0, 255, 217,
];
const WEBP_1X1: &[u8] = &[
    82, 73, 70, 70, 60, 0, 0, 0, 87, 69, 66, 80, 86, 80, 56, 32, 48, 0, 0, 0, 208, 1, 0, 157, 1,
    42, 1, 0, 1, 0, 1, 64, 38, 37, 160, 2, 116, 186, 1, 248, 0, 3, 176, 0, 254, 242, 235, 127, 252,
    216, 21, 205, 115, 239, 247, 255, 210, 224, 253, 46, 15, 210, 224, 255, 210, 144, 0, 0,
];

fn image_value_with(media_type: &str, bytes: &[u8], width: u32, height: u32) -> Value {
    json!({
        "type": "image",
        "image": {
            "bytes": bytes,
            "media_type": media_type,
            "width": width,
            "height": height
        }
    })
}

fn image_value() -> Value {
    image_value_with("png", PNG_1X1, 1, 1)
}

fn request_with_image(image: Value) -> GenRequest {
    serde_json::from_value(json!({
        "model": "fixture-vision",
        "messages": [{
            "role": "user",
            "content": [image, {"type": "text", "text": "describe it"}]
        }],
        "tools": [],
        "thinking": "off",
        "max_output_tokens": 64,
        "parallel_tool_calls": false
    }))
    .expect("the image request is valid")
}

fn capabilities(image_input: Value) -> ModelCapabilities {
    let image_input = match image_input {
        Value::Object(mut limits) if !limits.contains_key("kind") => {
            limits.insert("kind".to_string(), json!("supported"));
            Value::Object(limits)
        }
        other => other,
    };
    serde_json::from_value(json!({
        "schema_version": 4,
        "max_context_tokens": null,
        "max_output_tokens": 1024,
        "max_tools": null,
        "supports_parallel_tool_calls": false,
        "reasoning_carryover": "unsupported",
        "reasoning_efforts": "any",
        "reasoning_state_dialect": null,
        "cancel": "cooperative",
        "cache": "none",
        "transport": "sse",
        "image_input": image_input
    }))
    .expect("capability schema v4 is valid")
}

#[test]
fn image_is_an_immutable_validated_value_with_no_origin_fields() {
    let image = image_value();
    let block: ego_core::generator::types::ContentBlock =
        serde_json::from_value(image).expect("a valid PNG image block decodes");
    let encoded = serde_json::to_value(block).expect("image block serialises");
    assert_eq!(encoded["type"], "image");
    assert_eq!(encoded["image"]["media_type"], "png");
    assert_eq!(encoded["image"]["width"], 1);
    assert_eq!(encoded["image"]["height"], 1);
    for forbidden in [
        "path",
        "url",
        "filename",
        "origin",
        "provider_file_id",
        "sha256",
    ] {
        assert!(
            encoded["image"].get(forbidden).is_none(),
            "image leaked acquisition fact {forbidden}: {encoded}"
        );
    }
}

#[test]
fn image_validation_rejects_malformed_bytes_media_dimensions_and_unknown_fields() {
    for image in [
        json!({"type":"image","image":{"bytes":[],"media_type":"png","width":1,"height":1}}),
        json!({"type":"image","image":{"bytes":[1,2,3],"media_type":"png","width":1,"height":1}}),
        json!({"type":"image","image":{"bytes":PNG_1X1,"media_type":"bmp","width":1,"height":1}}),
        json!({"type":"image","image":{"bytes":PNG_1X1,"media_type":"png","width":0,"height":1}}),
        json!({"type":"image","image":{"bytes":PNG_1X1,"media_type":"png","width":1,"height":0}}),
        json!({"type":"image","image":{"bytes":PNG_1X1,"media_type":"png","width":1,"height":1,"url":"https://example.invalid/a.png"}}),
    ] {
        assert!(
            serde_json::from_value::<ego_core::generator::types::ContentBlock>(image).is_err(),
            "malformed image value was admitted"
        );
    }
}

#[test]
fn a_24_byte_png_signature_and_ihdr_prefix_is_still_truncated() {
    let prefix = &PNG_1X1[..24];
    assert_eq!(prefix.len(), 24);
    assert!(
        serde_json::from_value::<ego_core::generator::types::ContentBlock>(image_value_with(
            "png", prefix, 1, 1
        ))
        .is_err(),
        "a PNG signature plus IHDR dimensions was admitted without image data"
    );
}

#[test]
fn admitted_image_formats_require_their_signature_and_nonzero_dimensions() {
    for (media_type, bytes) in [
        ("jpeg", JPEG_1X1),
        ("png", PNG_1X1),
        ("gif", GIF_1X1),
        ("webp", WEBP_1X1),
    ] {
        let image = image_value_with(media_type, bytes, 1, 1);
        let block: ego_core::generator::types::ContentBlock = serde_json::from_value(image)
            .unwrap_or_else(|error| panic!("valid {media_type} signature was rejected: {error}"));
        let encoded = serde_json::to_value(block).unwrap();
        assert_eq!(encoded["image"]["media_type"], media_type);
        assert_eq!(encoded["image"]["width"], 1);
        assert_eq!(encoded["image"]["height"], 1);
    }
}

fn png_with_dimensions(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = PNG_1X1.to_vec();
    bytes[16..20].copy_from_slice(&width.to_be_bytes());
    bytes[20..24].copy_from_slice(&height.to_be_bytes());
    let mut crc = 0xffff_ffff_u32;
    for byte in &bytes[12..29] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
    bytes
}

#[test]
fn image_dimensions_are_derived_from_the_encoded_bytes() {
    let bytes = png_with_dimensions(7, 11);
    let image = image_value_with("png", &bytes, 7, 11);
    let block: ego_core::generator::types::ContentBlock =
        serde_json::from_value(image).expect("encoded dimensions are valid");
    let encoded = serde_json::to_value(block).unwrap();
    assert_eq!(encoded["image"]["width"], 7);
    assert_eq!(encoded["image"]["height"], 11);
    let mismatched = image_value_with("png", &bytes, 1, 1);
    assert!(
        serde_json::from_value::<ego_core::generator::types::ContentBlock>(mismatched).is_err()
    );
}

#[test]
fn animated_gif_is_refused_even_when_its_first_frame_is_admitted() {
    let mut animated = GIF_1X1[..GIF_1X1.len() - 1].to_vec();
    animated.extend_from_slice(&GIF_1X1[19..GIF_1X1.len() - 1]);
    animated.push(59);
    assert!(
        serde_json::from_value::<ego_core::generator::types::ContentBlock>(image_value_with(
            "gif", &animated, 1, 1
        ))
        .is_err(),
        "animated GIF was accepted as a single-frame image"
    );
}

#[test]
fn images_are_legal_only_in_user_messages_and_preserve_image_then_text_order() {
    let image = image_value();
    let request = request_with_image(image.clone());
    let content = &request.messages.messages()[0].content;
    assert_eq!(content[0], serde_json::from_value(image).unwrap());
    assert!(
        matches!(&content[1], ego_core::generator::types::ContentBlock::Text { text } if text == "describe it")
    );

    for role in ["system", "assistant"] {
        let invalid = serde_json::from_value::<Conversation>(json!([
            {"role": role, "content": [image_value()]}
        ]));
        assert!(invalid.is_err(), "image in {role} message was accepted");
    }
}

#[test]
fn capability_schema_v4_is_fail_closed_and_old_records_decode_as_unknown() {
    let unknown = capabilities(json!("unknown"));
    let unsupported = capabilities(json!("unsupported"));
    let supported = capabilities(json!({
        "accepted_media_types": ["png", "jpeg"],
        "max_images": 2,
        "max_bytes_per_image": 1024,
        "max_total_bytes": 2048,
        "max_width": 4096,
        "max_height": 4096,
        "max_pixels": 16_777_216
    }));
    for record in [unknown, unsupported, supported] {
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(encoded["schema_version"], 4);
        assert!(encoded.get("image_input").is_some());
    }

    let mut old = serde_json::to_value(capabilities(json!("unknown"))).unwrap();
    old["schema_version"] = json!(3);
    old.as_object_mut().unwrap().remove("image_input");
    let decoded: ModelCapabilities = serde_json::from_value(old).expect("v3 is readable");
    let encoded = serde_json::to_value(decoded).unwrap();
    assert_eq!(encoded["image_input"], "unknown");
}

#[test]
fn image_request_gaps_report_unknown_unsupported_media_count_bytes_dimensions_pixels_in_order() {
    let request = request_with_image(image_value());
    let cases = [
        (capabilities(json!("unknown")), "unknown image input"),
        (capabilities(json!("unsupported")), "does not accept images"),
        (
            // The refusal names the encoding the way the world spells it,
            // rather than the way `rustc` spells the variant.
            capabilities(json!({"accepted_media_types":["jpeg"]})),
            "image/png",
        ),
        (
            capabilities(json!({"accepted_media_types":["png"],"max_bytes_per_image":1})),
            "bytes",
        ),
    ];
    for (caps, expected) in cases {
        let gaps: Vec<CapabilityGap> = caps.request_gaps(&request);
        let rendered = gaps
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(!gaps.is_empty(), "image request was admitted: {rendered}");
        assert!(
            rendered.to_ascii_lowercase().contains(expected),
            "gap omitted {expected}: {rendered}"
        );
    }

    let two_images: GenRequest = serde_json::from_value(json!({
        "model":"fixture-vision",
        "messages":[{"role":"user","content":[image_value(), image_value()]}],
        "tools":[],"thinking":"off","max_output_tokens":64,"parallel_tool_calls":false
    }))
    .expect("two image request");
    let count_gaps = capabilities(json!({"accepted_media_types":["png"],"max_images":1}))
        .request_gaps(&two_images);
    assert!(
        count_gaps
            .iter()
            .any(|gap| gap.to_string().contains("image"))
    );
    let total_gaps = capabilities(json!({"accepted_media_types":["png"],"max_total_bytes":1}))
        .request_gaps(&two_images);
    assert!(
        total_gaps
            .iter()
            .any(|gap| gap.to_string().contains("total"))
    );

    let wide_bytes = png_with_dimensions(2, 1);
    let wide_request = request_with_image(image_value_with("png", &wide_bytes, 2, 1));
    let dimension_gaps = capabilities(json!({"accepted_media_types":["png"],"max_width":1}))
        .request_gaps(&wide_request);
    assert!(
        dimension_gaps
            .iter()
            .any(|gap| gap.to_string().contains("width"))
    );
    let tall_bytes = png_with_dimensions(1, 2);
    let tall_request = request_with_image(image_value_with("png", &tall_bytes, 1, 2));
    let height_gaps = capabilities(json!({"accepted_media_types":["png"],"max_height":1}))
        .request_gaps(&tall_request);
    assert!(
        height_gaps
            .iter()
            .any(|gap| gap.to_string().contains("height"))
    );
    let pixel_gaps = capabilities(json!({"accepted_media_types":["png"],"max_pixels":1}))
        .request_gaps(&wide_request);
    assert!(
        pixel_gaps
            .iter()
            .any(|gap| gap.to_string().contains("pixel"))
    );
}

/// The refusal says where the images came from, because the person reading it
/// only wrote the last message.
///
/// `request_gaps` scans the whole request, and it is right to: the history is
/// resent on every turn, so historical images really are part of what a
/// text-only model would have to accept. But the refusal used to say
/// "unsupported image input" on a prompt that supplied none — reachable by
/// `ego --attach` followed by a mid-session model change — and the person had
/// no way to tell that the image was one they sent four turns ago.
///
/// The two halves also have different remedies, which is the reason the count
/// is split rather than merely mentioned: an attachment on this prompt can be
/// dropped, and one already in the conversation cannot be.
#[test]
fn a_refusal_names_the_images_the_prompt_sent_apart_from_the_ones_it_inherited() {
    let text_only = capabilities(json!("unsupported"));

    let carried: GenRequest = serde_json::from_value(json!({
        "model":"fixture-vision",
        "messages":[
            {"role":"user","content":[image_value(), {"type":"text","text":"describe it"}]},
            {"role":"assistant","content":[{"type":"text","text":"a dot"}]},
            {"role":"user","content":[{"type":"text","text":"and now in French"}]}
        ],
        "tools":[],"thinking":"off","max_output_tokens":64,"parallel_tool_calls":false
    }))
    .expect("a conversation that already contains an image");
    let rendered = text_only
        .request_gaps(&carried)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        rendered.contains("already carries 1") && !rendered.contains("this prompt supplied"),
        "a text-only prompt was told it sent an image: {rendered}"
    );
    // The remedy is the half a bare count would have lost.
    assert!(
        rendered.contains("start a new session"),
        "the refusal names no way out: {rendered}"
    );

    let supplied = text_only
        .request_gaps(&request_with_image(image_value()))
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        supplied.contains("this prompt supplied 1") && !supplied.contains("already carries"),
        "an image sent now was blamed on the history: {supplied}"
    );
}

#[test]
fn product_image_caps_are_hard_bounds_without_checked_in_large_fixtures() {
    let caps = capabilities(json!({
        "kind":"supported",
        "accepted_media_types":["png"],
        "max_images":100,
        "max_bytes_per_image":67108864,
        "max_total_bytes":67108864,
        "max_width":16384,
        "max_height":16384,
        "max_pixels":100000000
    }));
    let many: GenRequest = serde_json::from_value(json!({
        "model":"fixture-vision",
        "messages":[{"role":"user","content":(0..101).map(|_| image_value()).collect::<Vec<_>>() }],
        "tools":[],"thinking":"off","max_output_tokens":64,"parallel_tool_calls":false
    }))
    .expect("101-image request shape");
    assert!(
        caps.request_gaps(&many)
            .iter()
            .any(|gap| gap.to_string().contains("100"))
    );

    let edge_bytes = png_with_dimensions(16385, 1);
    let pixel_bytes = png_with_dimensions(10000, 10001);
    assert!(
        serde_json::from_value::<ego_core::generator::types::ContentBlock>(image_value_with(
            "png",
            &edge_bytes,
            16385,
            1
        ))
        .is_err(),
        "image exceeded the absolute 16384 edge cap"
    );
    assert!(
        serde_json::from_value::<ego_core::generator::types::ContentBlock>(image_value_with(
            "png",
            &pixel_bytes,
            10000,
            10001
        ))
        .is_err(),
        "image exceeded the absolute 100000000-pixel cap"
    );

    for (field, value) in [
        ("max_images", 101_u64),
        ("max_bytes_per_image", 67_108_865_u64),
        ("max_total_bytes", 67_108_865_u64),
        ("max_width", 16_385_u64),
        ("max_height", 16_385_u64),
        ("max_pixels", 100_000_001_u64),
    ] {
        let mut record = json!({
            "schema_version":4,"max_context_tokens":null,"max_output_tokens":1024,
            "max_tools":null,"supports_parallel_tool_calls":false,
            "reasoning_carryover":"unsupported","reasoning_efforts":"any",
            "reasoning_state_dialect":null,"cancel":"cooperative","cache":"none",
            "transport":"sse","image_input":{"kind":"supported", "accepted_media_types":["png"]}
        });
        record["image_input"][field] = json!(value);
        assert!(
            serde_json::from_value::<ModelCapabilities>(record).is_err(),
            "provider image capability raised product bound {field}"
        );
    }
}

#[test]
fn capability_limits_are_nonzero_when_present() {
    for field in [
        "max_images",
        "max_bytes_per_image",
        "max_total_bytes",
        "max_width",
        "max_height",
        "max_pixels",
    ] {
        let mut record = json!({
            "schema_version":4,"max_context_tokens":null,"max_output_tokens":1024,
            "max_tools":null,"supports_parallel_tool_calls":false,
            "reasoning_carryover":"unsupported","reasoning_efforts":"any",
            "reasoning_state_dialect":null,"cancel":"cooperative","cache":"none",
            "transport":"sse","image_input":{"kind":"supported", "accepted_media_types":["png"]}
        });
        record["image_input"][field] = json!(0);
        assert!(
            serde_json::from_value::<ModelCapabilities>(record).is_err(),
            "zero image capability limit {field} was accepted"
        );
    }
}

#[test]
fn durable_user_prompt_keeps_ordered_blob_references_and_old_records_remain_readable() {
    let old: EventKind = serde_json::from_value(json!({
        "kind": "UserPrompt",
        "text": "old prompt"
    }))
    .expect("old user prompt records remain readable");
    let old_wire = serde_json::to_value(old).unwrap();
    assert_eq!(old_wire, json!({"kind":"UserPrompt","text":"old prompt"}));

    let current: EventKind = serde_json::from_value(json!({
        "kind": "UserPrompt",
        "text": "with images",
        "images": [
            {"reference": format!("{SHA256}:3"), "media_type":"png", "width":1, "height":1},
            {"reference": format!("{SHA256}:5"), "media_type":"jpeg", "width":2, "height":2}
        ]
    }))
    .expect("image references are durable prompt metadata");
    let encoded = serde_json::to_value(current).unwrap();
    assert_eq!(encoded["images"][0]["reference"], format!("{SHA256}:3"));
    assert_eq!(encoded["images"][1]["reference"], format!("{SHA256}:5"));
    assert!(
        !encoded.to_string().contains("137,80,78,71"),
        "durable records inlined image bytes"
    );
}

#[test]
fn queued_prompt_and_resume_metadata_preserve_blob_order_without_reacquisition() {
    let id = QueuedPromptId::new();
    let queued: EventKind = serde_json::from_value(json!({
        "kind": "PromptQueued",
        "id": id,
        "text": "queued image prompt",
        "images": [
            {"reference": format!("{SHA256}:3"), "media_type":"png", "width":1, "height":1},
            {"reference": format!("{SHA256}:5"), "media_type":"webp", "width":2, "height":2}
        ]
    }))
    .expect("queued prompt image metadata is durable");
    let encoded = serde_json::to_value(queued).unwrap();
    assert_eq!(encoded["id"], serde_json::to_value(id).unwrap());
    assert_eq!(encoded["images"][0]["media_type"], "png");
    assert_eq!(encoded["images"][1]["media_type"], "webp");
    assert!(
        encoded.get("url").is_none(),
        "queue persisted an acquisition origin"
    );
}

#[test]
fn malformed_durable_blob_references_are_rejected_before_replay() {
    let references = vec![
        "not-a-sha:3".to_string(),
        format!("{SHA256}:0"),
        format!("{SHA256}:18446744073709551616"),
    ];
    for reference in references {
        let record = json!({
            "kind":"UserPrompt",
            "text":"replay",
            "images":[{"reference":reference,"media_type":"png","width":1,"height":1}]
        });
        assert!(
            serde_json::from_value::<EventKind>(record).is_err(),
            "malformed blob reference was replayed or reacquired: {reference}"
        );
    }
}
