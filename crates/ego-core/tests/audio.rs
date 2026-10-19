//! Audio domain contracts. Fixture generated with Python's standard wave encoder.
use ego_core::generator::audio::{Audio, AudioMediaType, AudioRef, MAX_AUDIO_BYTES};
use ego_core::generator::capabilities::{AudioInputSupport, CapabilityGap, ModelCapabilities};
use ego_core::generator::types::{ContentBlock, GenRequest};
use ego_core::store::blob::{BlobId, BlobRef};
use serde_json::json;

const WAV: &[u8] = include_bytes!("fixtures/audio/pcm16-mono.wav");

// Catches accepting an origin or contradictory encoding instead of immutable WAV bytes.
#[test]
fn audio_roundtrip_preserves_exact_validated_bytes_and_refuses_origin() {
    let audio = Audio::new(WAV.to_vec(), AudioMediaType::Wav).unwrap();
    let value = serde_json::to_value(&audio).unwrap();
    assert_eq!(
        serde_json::from_value::<Audio>(value.clone()).unwrap(),
        audio
    );
    let mut origin = value;
    origin["url"] = json!("https://example.invalid/clip.wav");
    assert!(serde_json::from_value::<Audio>(origin).is_err());
    assert_eq!(audio.bytes(), WAV);
}

// Catches trusting RIFF lengths, sample declarations or host-controlled chunk sizes.
#[test]
fn malformed_or_out_of_contract_wav_is_refused() {
    let mut cases = vec![vec![], WAV[..43].to_vec(), vec![0; MAX_AUDIO_BYTES + 1]];
    for (offset, replacement) in [(4, 0_u32), (24, 48_001), (40, u32::MAX)] {
        let mut bytes = WAV.to_vec();
        bytes[offset..offset + 4].copy_from_slice(&replacement.to_le_bytes());
        cases.push(bytes);
    }
    for offset in [20, 22, 32, 34] {
        let mut bytes = WAV.to_vec();
        bytes[offset..offset + 2].copy_from_slice(&3_u16.to_le_bytes());
        cases.push(bytes);
    }
    // A tiny sample rate would make this clip longer than sixty seconds.
    let mut too_long = WAV.to_vec();
    too_long[24..28].copy_from_slice(&1_u32.to_le_bytes());
    too_long[28..32].copy_from_slice(&2_u32.to_le_bytes());
    cases.push(too_long);
    for bytes in cases {
        assert!(Audio::new(bytes, AudioMediaType::Wav).is_err());
    }
}

// Catches replay trusting reference length without revalidating the codec.
#[test]
fn durable_audio_ref_refuses_wrong_length_and_invalid_encoding() {
    let reference = AudioRef::new(
        BlobRef::new(BlobId::parse("0".repeat(64)).unwrap(), WAV.len() as u64),
        AudioMediaType::Wav,
    )
    .unwrap();
    assert_eq!(reference.hydrate(WAV.to_vec()).unwrap().bytes(), WAV);
    assert!(reference.hydrate(WAV[..WAV.len() - 1].to_vec()).is_err());
    assert!(reference.hydrate(vec![0; WAV.len()]).is_err());
}

fn request() -> GenRequest {
    serde_json::from_value(json!({
        "model":"gpt-audio-1.5", "messages":[{"role":"user","content":[
            {"type":"audio","audio":{"bytes":WAV,"media_type":"wav"}}
        ]}], "tools":[], "thinking":"off", "parallel_tool_calls":false
    }))
    .unwrap()
}

fn caps() -> ModelCapabilities {
    serde_json::from_value(json!({"schema_version":ModelCapabilities::SCHEMA_VERSION,
        "supports_parallel_tool_calls":false,"reasoning_carryover":"unsupported",
        "reasoning_efforts":"any","cancel":"cooperative","cache":"none","transport":"sse"
    }))
    .unwrap()
}

// Catches a model switch or replay silently dropping historical audio capability checks.
#[test]
fn historical_audio_is_refused_by_unknown_and_unsupported_models() {
    let req = request();
    let mut caps = caps();
    assert!(
        caps.request_gaps(&req)
            .contains(&CapabilityGap::AudioInputUnknown)
    );
    caps.audio_input = AudioInputSupport::Unsupported;
    assert!(
        caps.request_gaps(&req)
            .contains(&CapabilityGap::AudioInputUnsupported)
    );
    caps.audio_input = AudioInputSupport::SupportedWav;
    assert!(caps.request_gaps(&req).is_empty());
}

// Catches permitting audio in assistant/tool roles through deserialization.
#[test]
fn only_user_conversation_messages_may_contain_audio() {
    for role in ["assistant", "tool", "system"] {
        let mut value = serde_json::to_value(request()).unwrap();
        value["messages"][0]["role"] = json!(role);
        assert!(serde_json::from_value::<GenRequest>(value).is_err());
    }
    assert!(matches!(
        request().messages.messages()[0].content[0],
        ContentBlock::Audio { .. }
    ));
}

// Catches text-token prices being treated as a complete bound on audio spending.
#[test]
fn text_token_quote_cannot_authorize_audio_spend() {
    use ego_core::budget::PricingQuote;
    use ego_core::cost::{Amount, CurrencyCode, PricingRevision, UnknownCost};
    let quote = PricingQuote {
        revision: PricingRevision("text-only".into()),
        currency: CurrencyCode::new("USD").unwrap(),
        web_search: None,
        uncached_input: Some(Amount::ZERO),
        cache_read: Some(Amount::ZERO),
        cache_write: Some(Amount::ZERO),
        cache_write_extended: None,
        output: Some(Amount::ZERO),
    };
    assert_eq!(
        quote.upper_bound(&request(), &caps()),
        Err(UnknownCost::AudioPricingUnavailable)
    );
}

// Catches pending queue replay/GC losing audio before the queued prompt is consumed.
#[test]
fn queued_audio_metadata_and_gc_roots_survive_event_roundtrip() {
    use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
    use ego_core::event::id::{EventId, Producer, QueuedPromptId, Seq, TurnId};
    use ego_core::event::time::Timestamp;
    use ego_core::state::SessionState;
    let reference = BlobRef::new(BlobId::parse("0".repeat(64)).unwrap(), WAV.len() as u64);
    let audio = AudioRef::new(reference.clone(), AudioMediaType::Wav).unwrap();
    let mut seq = Seq::ZERO;
    let event = DomainEvent {
        env: Envelope {
            seq: seq.next(),
            id: EventId::new(),
            turn: TurnId::new(),
            item: None,
            parent: None,
            ts: Timestamp::from_millis(0),
            producer: Producer::Main,
        },
        kind: EventKind::PromptQueued {
            id: QueuedPromptId::new(),
            text: String::new(),
            images: Vec::new(),
            audio: vec![audio.clone()],
        },
    };
    let decoded: DomainEvent =
        serde_json::from_slice(&serde_json::to_vec(&event).unwrap()).unwrap();
    let mut state = SessionState::default();
    state.apply(&decoded);
    assert_eq!(state.queued_prompts()[0].audio(), &[audio]);
    assert!(state.blob_references().contains(&reference));
}
