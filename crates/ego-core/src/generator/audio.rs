//! Exact bounded audio input and its durable session-blob reference.

use serde::{Deserialize, Serialize};

use crate::store::blob::BlobRef;

pub const MAX_AUDIO_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_AUDIO_PER_PROMPT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioMediaType {
    Wav,
}

impl AudioMediaType {
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Wav => "audio/wav",
        }
    }
}

/// Constructed only after validating the exact encoding; never an origin URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "AudioWire")]
pub struct Audio {
    bytes: Box<[u8]>,
    media_type: AudioMediaType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text_offset: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioWire {
    bytes: Vec<u8>,
    media_type: AudioMediaType,
    #[serde(default)]
    text_offset: Option<usize>,
}
impl TryFrom<AudioWire> for Audio {
    type Error = String;
    fn try_from(value: AudioWire) -> Result<Self, String> {
        Self::new(value.bytes, value.media_type)
            .map(|audio| audio.with_text_offset(value.text_offset))
    }
}

impl Audio {
    pub fn new(bytes: Vec<u8>, media_type: AudioMediaType) -> Result<Self, String> {
        validate_wav(&bytes)?;
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            media_type,
            text_offset: None,
        })
    }

    pub fn with_text_offset(mut self, offset: Option<usize>) -> Self {
        self.text_offset = offset;
        self
    }
    pub const fn text_offset(&self) -> Option<usize> {
        self.text_offset
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub const fn media_type(&self) -> AudioMediaType {
        self.media_type
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "AudioRefWire")]
pub struct AudioRef {
    reference: BlobRef,
    media_type: AudioMediaType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text_offset: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioRefWire {
    reference: BlobRef,
    media_type: AudioMediaType,
    #[serde(default)]
    text_offset: Option<usize>,
}

impl TryFrom<AudioRefWire> for AudioRef {
    type Error = String;
    fn try_from(value: AudioRefWire) -> Result<Self, String> {
        Self::new(value.reference, value.media_type)
            .map(|reference| reference.with_text_offset(value.text_offset))
    }
}

impl AudioRef {
    pub fn new(reference: BlobRef, media_type: AudioMediaType) -> Result<Self, String> {
        if reference.bytes == 0 || reference.bytes > MAX_AUDIO_BYTES as u64 {
            return Err("audio blob exceeds the nonempty 10 MiB product bound".into());
        }
        Ok(Self {
            reference,
            media_type,
            text_offset: None,
        })
    }
    pub fn with_text_offset(mut self, offset: Option<usize>) -> Self {
        self.text_offset = offset;
        self
    }
    pub const fn text_offset(&self) -> Option<usize> {
        self.text_offset
    }
    pub fn reference(&self) -> &BlobRef {
        &self.reference
    }
    pub const fn media_type(&self) -> AudioMediaType {
        self.media_type
    }
    pub fn hydrate(&self, bytes: Vec<u8>) -> Result<Audio, String> {
        if bytes.len() as u64 != self.reference.bytes {
            return Err("audio blob length does not match its durable reference".into());
        }
        Audio::new(bytes, self.media_type).map(|audio| audio.with_text_offset(self.text_offset))
    }
}

/// The product admits PCM16 WAV, mono/stereo, at most 48 kHz and 60 seconds.
/// Chunk sizes and RIFF length are checked before slicing; unknown chunks are
/// preserved, not rewritten. No decoder allocation follows host-supplied sizes.
fn validate_wav(bytes: &[u8]) -> Result<(), String> {
    let invalid =
        || "audio must be a complete PCM16 WAV (mono/stereo, <=48 kHz, <=60 seconds)".to_owned();
    if bytes.len() > MAX_AUDIO_BYTES || bytes.len() < 44 {
        return Err("audio must be nonempty and at most 10 MiB".into());
    }
    let u32_at = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    if &bytes[..4] != b"RIFF"
        || &bytes[8..12] != b"WAVE"
        || u32_at(4) as u64 + 8 != bytes.len() as u64
    {
        return Err(invalid());
    }
    let mut offset = 12usize;
    let mut format = None;
    let mut data_len = None;
    while offset < bytes.len() {
        if bytes.len() - offset < 8 {
            return Err(invalid());
        }
        let length = u32_at(offset + 4) as usize;
        let start = offset + 8;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(invalid)?;
        match &bytes[offset..offset + 4] {
            b"fmt " => {
                if format.is_some() || length != 16 {
                    return Err(invalid());
                }
                let u16_at =
                    |n| u16::from_le_bytes(bytes[start + n..start + n + 2].try_into().unwrap());
                let channels = u16_at(2);
                let rate = u32_at(start + 4);
                let byte_rate = u32_at(start + 8);
                let align = u16_at(12);
                if u16_at(0) != 1
                    || !(1..=2).contains(&channels)
                    || u16_at(14) != 16
                    || !(1..=48_000).contains(&rate)
                    || align != channels * 2
                    || byte_rate != rate * u32::from(align)
                {
                    return Err(invalid());
                }
                format = Some((byte_rate, align));
            }
            b"data" => {
                if data_len.is_some() || format.is_none() || length == 0 {
                    return Err(invalid());
                }
                data_len = Some(length);
            }
            _ => {}
        }
        offset = end
            .checked_add(length % 2)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(invalid)?;
    }
    let (byte_rate, align) = format.ok_or_else(invalid)?;
    let data_len = data_len.ok_or_else(invalid)?;
    if data_len % usize::from(align) != 0 || data_len as u64 > u64::from(byte_rate) * 60 {
        return Err(invalid());
    }
    Ok(())
}
