//! The provider-neutral conversation model: `GenRequest`, `Conversation`,
//! `Message`, `ContentBlock`, `ToolDescriptor`, `StopReason`, `ErrorKind`,
//! `ThinkingLevel`, `ReasoningState`.
//!
//! See `ARCHITECTURE.md` §3.1 and `ARCHITECTURE.md` §2 (the pi taxonomy this
//! borrows from), and `ARCHITECTURE.md` for why
//! a message is a list of blocks rather than a string plus an optional id.

use std::collections::BTreeMap;
use std::io::Cursor;

use serde::{Deserialize, Serialize};

use crate::store::blob::BlobRef;

pub const MAX_IMAGES_PER_PROMPT: usize = 100;
pub const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_TOTAL_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_IMAGE_EDGE: u32 = 16_384;
pub const MAX_IMAGE_PIXELS: u64 = 100_000_000;

/// The closed set of exact image encodings the provider-neutral model admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageMediaType {
    Jpeg,
    Png,
    Gif,
    Webp,
}

impl ImageMediaType {
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

/// Exact immutable bytes plus dimensions proved from those bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    bytes: Box<[u8]>,
    media_type: ImageMediaType,
    width: u32,
    height: u32,
}

impl Image {
    pub fn new(bytes: Vec<u8>, media_type: ImageMediaType) -> Result<Self, ImageError> {
        validate_image_byte_bounds(bytes.len())?;
        let (width, height) = decoded_dimensions(&bytes, media_type)?;
        validate_image_dimension_bounds(width, height)?;
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            media_type,
            width,
            height,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn media_type(&self) -> ImageMediaType {
        self.media_type
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageWire {
    bytes: Vec<u8>,
    media_type: ImageMediaType,
    width: u32,
    height: u32,
}

impl<'de> Deserialize<'de> for Image {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ImageWire::deserialize(deserializer)?;
        let image = Self::new(wire.bytes, wire.media_type).map_err(serde::de::Error::custom)?;
        if image.width != wire.width || image.height != wire.height {
            return Err(serde::de::Error::custom(format!(
                "image declares {}x{} but its bytes decode as {}x{}",
                wire.width, wire.height, image.width, image.height
            )));
        }
        Ok(image)
    }
}

/// Durable image metadata. Acquisition origins and bytes never enter it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRef {
    reference: BlobRef,
    media_type: ImageMediaType,
    width: u32,
    height: u32,
}

impl ImageRef {
    pub fn new(
        reference: BlobRef,
        media_type: ImageMediaType,
        width: u32,
        height: u32,
    ) -> Result<Self, ImageError> {
        if reference.bytes == 0 {
            return Err(ImageError::Empty);
        }
        validate_image_bounds(
            usize::try_from(reference.bytes).unwrap_or(usize::MAX),
            width,
            height,
        )?;
        Ok(Self {
            reference,
            media_type,
            width,
            height,
        })
    }

    pub fn hydrate(&self, bytes: Vec<u8>) -> Result<Image, ImageError> {
        let image = Image::new(bytes, self.media_type)?;
        if image.width != self.width || image.height != self.height {
            return Err(ImageError::DimensionMismatch {
                declared_width: self.width,
                declared_height: self.height,
                decoded_width: image.width,
                decoded_height: image.height,
            });
        }
        Ok(image)
    }

    pub fn reference(&self) -> &BlobRef {
        &self.reference
    }
    pub const fn media_type(&self) -> ImageMediaType {
        self.media_type
    }
    pub const fn width(&self) -> u32 {
        self.width
    }
    pub const fn height(&self) -> u32 {
        self.height
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageRefWire {
    reference: BlobRef,
    media_type: ImageMediaType,
    width: u32,
    height: u32,
}

impl<'de> Deserialize<'de> for ImageRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ImageRefWire::deserialize(deserializer)?;
        Self::new(wire.reference, wire.media_type, wire.width, wire.height)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    Empty,
    TooLarge {
        bytes: u64,
    },
    InvalidEncoding {
        media_type: ImageMediaType,
    },
    AnimatedGif,
    InvalidDimensions {
        width: u32,
        height: u32,
    },
    TooManyPixels {
        pixels: u64,
    },
    DimensionMismatch {
        declared_width: u32,
        declared_height: u32,
        decoded_width: u32,
        decoded_height: u32,
    },
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "an image cannot be empty"),
            Self::TooLarge { bytes } => write!(
                f,
                "image has {bytes} bytes, above the {MAX_IMAGE_BYTES}-byte product limit"
            ),
            Self::InvalidEncoding { media_type } => {
                write!(f, "bytes are not a valid {media_type:?} image")
            }
            Self::AnimatedGif => write!(f, "animated GIF input is not supported"),
            Self::InvalidDimensions { width, height } => write!(
                f,
                "image dimensions {width}x{height} are outside the 1..={MAX_IMAGE_EDGE} product bound"
            ),
            Self::TooManyPixels { pixels } => write!(
                f,
                "image has {pixels} pixels, above the {MAX_IMAGE_PIXELS}-pixel product limit"
            ),
            Self::DimensionMismatch {
                declared_width,
                declared_height,
                decoded_width,
                decoded_height,
            } => write!(
                f,
                "image declares {declared_width}x{declared_height} but its bytes decode as {decoded_width}x{decoded_height}"
            ),
        }
    }
}

impl std::error::Error for ImageError {}

fn validate_image_bounds(bytes: usize, width: u32, height: u32) -> Result<(), ImageError> {
    validate_image_byte_bounds(bytes)?;
    validate_image_dimension_bounds(width, height)
}

fn validate_image_byte_bounds(bytes: usize) -> Result<(), ImageError> {
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    if bytes == 0 {
        return Err(ImageError::Empty);
    }
    if bytes > MAX_IMAGE_BYTES {
        return Err(ImageError::TooLarge { bytes });
    }
    Ok(())
}

fn validate_image_dimension_bounds(width: u32, height: u32) -> Result<(), ImageError> {
    if width == 0 || height == 0 || width > MAX_IMAGE_EDGE || height > MAX_IMAGE_EDGE {
        return Err(ImageError::InvalidDimensions { width, height });
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(ImageError::TooManyPixels { pixels: u64::MAX })?;
    if pixels > MAX_IMAGE_PIXELS {
        return Err(ImageError::TooManyPixels { pixels });
    }
    Ok(())
}

fn decoded_dimensions(bytes: &[u8], media_type: ImageMediaType) -> Result<(u32, u32), ImageError> {
    let invalid = || ImageError::InvalidEncoding { media_type };
    match media_type {
        ImageMediaType::Png => png_dimensions(bytes, media_type),
        ImageMediaType::Jpeg => jpeg_dimensions(bytes).ok_or_else(invalid),
        ImageMediaType::Gif => gif_dimensions(bytes).map_err(|animated| {
            if animated {
                ImageError::AnimatedGif
            } else {
                invalid()
            }
        }),
        ImageMediaType::Webp => webp_dimensions(bytes).ok_or_else(invalid),
    }
}

fn png_dimensions(bytes: &[u8], media_type: ImageMediaType) -> Result<(u32, u32), ImageError> {
    let invalid = || ImageError::InvalidEncoding { media_type };
    let limits = png::Limits {
        bytes: usize::try_from(MAX_IMAGE_BYTES).expect("the image byte cap fits in usize"),
    };
    let decoder = png::Decoder::new_with_limits(Cursor::new(bytes), limits);
    let mut reader = decoder.read_info().map_err(|_| invalid())?;
    let dimensions = (reader.info().width, reader.info().height);

    // Refuse impossible dimensions before consuming compressed pixel data.
    // `finish` then validates the complete IDAT stream and every remaining
    // chunk through IEND without allocating a decoded frame buffer.
    validate_image_dimension_bounds(dimensions.0, dimensions.1)?;
    reader.finish().map_err(|_| invalid())?;
    Ok(dimensions)
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return None;
    }
    let mut at = 2;
    while at + 3 < bytes.len() {
        if bytes[at] != 0xff {
            return None;
        }
        while at < bytes.len() && bytes[at] == 0xff {
            at += 1;
        }
        let marker = *bytes.get(at)?;
        at += 1;
        if marker == 0xd9 || marker == 0xda {
            break;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length = u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as usize;
        if length < 2 || at.checked_add(length)? > bytes.len() {
            return None;
        }
        let is_sof = matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
        if is_sof {
            if length < 7 {
                return None;
            }
            let height = u16::from_be_bytes(bytes[at + 3..at + 5].try_into().ok()?) as u32;
            let width = u16::from_be_bytes(bytes[at + 5..at + 7].try_into().ok()?) as u32;
            return Some((width, height));
        }
        at += length;
    }
    None
}

fn gif_dimensions(bytes: &[u8]) -> Result<(u32, u32), bool> {
    if bytes.len() < 14 || (!bytes.starts_with(b"GIF87a") && !bytes.starts_with(b"GIF89a")) {
        return Err(false);
    }
    let width = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
    let height = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
    let packed = bytes[10];
    let mut at = 13
        + if packed & 0x80 != 0 {
            3 * (1usize << (usize::from(packed & 7) + 1))
        } else {
            0
        };
    let mut frames = 0;
    while at < bytes.len() {
        match bytes[at] {
            0x3b => {
                return if frames == 1 {
                    Ok((width, height))
                } else {
                    Err(false)
                };
            }
            0x21 => {
                at = at.checked_add(2).ok_or(false)?;
                skip_gif_sub_blocks(bytes, &mut at)?;
            }
            0x2c => {
                frames += 1;
                if frames > 1 {
                    return Err(true);
                }
                if at + 10 > bytes.len() {
                    return Err(false);
                }
                let local = bytes[at + 9];
                at += 10;
                if local & 0x80 != 0 {
                    at = at
                        .checked_add(3 * (1usize << (usize::from(local & 7) + 1)))
                        .ok_or(false)?;
                }
                at = at.checked_add(1).ok_or(false)?;
                skip_gif_sub_blocks(bytes, &mut at)?;
            }
            _ => return Err(false),
        }
    }
    Err(false)
}

fn skip_gif_sub_blocks(bytes: &[u8], at: &mut usize) -> Result<(), bool> {
    loop {
        let length = usize::from(*bytes.get(*at).ok_or(false)?);
        *at += 1;
        if length == 0 {
            return Ok(());
        }
        *at = (*at).checked_add(length).ok_or(false)?;
        if *at > bytes.len() {
            return Err(false);
        }
    }
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 20 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let declared = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize;
    if declared.checked_add(8)? != bytes.len() {
        return None;
    }
    let kind = &bytes[12..16];
    let length = u32::from_le_bytes(bytes[16..20].try_into().ok()?) as usize;
    let payload = bytes.get(20..20usize.checked_add(length)?)?;
    match kind {
        b"VP8 " if payload.len() >= 10 && payload[3..6] == [0x9d, 0x01, 0x2a] => {
            let width = u16::from_le_bytes([payload[6], payload[7]]) & 0x3fff;
            let height = u16::from_le_bytes([payload[8], payload[9]]) & 0x3fff;
            Some((u32::from(width), u32::from(height)))
        }
        b"VP8L" if payload.len() >= 5 && payload[0] == 0x2f => {
            let bits = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8X" if payload.len() >= 10 => {
            let width = u32::from(payload[4])
                | (u32::from(payload[5]) << 8)
                | (u32::from(payload[6]) << 16);
            let height = u32::from(payload[7])
                | (u32::from(payload[8]) << 8)
                | (u32::from(payload[9]) << 16);
            Some((width + 1, height + 1))
        }
        _ => None,
    }
}

/// Reasoning effort, pi's scale (`ARCHITECTURE.md` §2) plus `Off`: pi actually
/// carries two related types (`ThinkingLevel` without `off`, and
/// `ModelThinkingLevel` adding it for per-model defaults). A `GenRequest`
/// needs to be able to say "no thinking" as well as an effort level, and the
/// plan asks for one `ThinkingLevel` type, so this merges the two rather than
/// introducing a second type the acceptance criteria doesn't ask for.
/// The ordering is the effort ordering, not an accident of declaration: it is
/// derived, and the variants are written from least to most so that deriving
/// it says something true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ThinkingLevel {
    /// The canonical spelling used by CLI and persisted configuration.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Why a generation stopped, pi's scale (`ARCHITECTURE.md` §2) minus `Pending`.
///
/// The one deliberate deviation from pi, decided in
/// `ARCHITECTURE.md`: pi's `Pending`
/// means "no terminal state yet", which is the *absence* of the answer this
/// type exists to give. Every variant here is terminal, so an
/// [`Outcome`](super::attempt::Outcome) cannot be built around a value that
/// says nothing ended — and it can be durable without a guard, because a
/// decoded `StopReason` has no non-terminal spelling to decode into.
///
/// "Not finished yet" is expressed by not having a `StopReason`: the stream
/// is still running, or `AttemptStream::terminal` is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// The model completed its turn normally.
    Stop,
    /// The output was truncated at a length limit.
    Length,
    /// The model stopped to call a tool.
    ToolUse,
    /// The provider reported an error.
    Error,
    /// The generation was cancelled.
    Aborted,
    /// The provider deferred delivery of the result (fetched later, not
    /// streamed) — `ARCHITECTURE.md` §2's `DeferredHandle`.
    Deferred,
}

/// Why a provider error happened, in the only terms a caller can act on.
///
/// Five values, each with a distinct action. Not the twenty-four of
/// OpenRouter's `error_type`: that is a fine vocabulary and it is
/// OpenRouter's, and six of its values describe image generation this
/// repository does not do. The adapter maps its provider's spelling onto
/// these; the mapping is data, and the branch stays in core.
///
/// Carrying the provider's own code as a string beside the message was
/// rejected for the reason that a string only helps if something matches on
/// it, and the only thing that would is `turn/` — matching on a provider's
/// spelling in `turn/` being exactly what
/// `ARCHITECTURE.md` §5's
/// name-absence test exists to forbid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Momentarily unable: overload, rate limit, timeout, upstream
    /// unavailability. A retry may succeed.
    Transient,
    /// The provider explicitly reported that the model context window was
    /// exhausted. The current request cannot succeed unchanged, and hosts
    /// may offer an explicit compaction transition.
    ContextOverflow,
    /// The request is wrong and will be wrong again: schema or an unsupported
    /// parameter. No retry changes it.
    Invalid,
    /// The account or the credential is refused: unauthenticated, forbidden,
    /// out of credit. No retry changes it, and the request was not the
    /// problem.
    Refused,
    /// The provider said something this mapping does not classify. Treated as
    /// non-retryable.
    ///
    /// The default, and it fails closed on the precedent
    /// [`ReasoningEffortSupport::Unknown`](super::capabilities::ReasoningEffortSupport::Unknown)
    /// set: a provider that stated nothing is not a provider that stated
    /// "retryable". Failing closed costs one attempt; failing open costs
    /// `--retry N` attempts and a wrong exit code.
    #[default]
    Unclassified,
}

/// Who authored a `Message`.
///
/// Three roles, not four: a tool *result* is a [`ContentBlock`] inside the
/// turn that carries it, not an author. The providers disagree about which
/// role that is — chat-completions gives a result its own `role: "tool"`
/// message, anthropic-messages puts it in a `user` message — so a `Tool` role
/// here would be one of them borrowed and imposed on the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// One piece of a message's content.
///
/// Ordered, because order is part of what makes a request valid: Anthropic
/// requires every `tool_result` of a turn to be grouped ahead of any text in
/// it, and requires a turn's thinking to precede its `tool_use`. A shape that
/// kept calls in a field beside the text would throw that ordering away and
/// leave each adapter to invent it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Visible text.
    Text { text: String },
    /// Exact image bytes. Only a user-authored message may carry this block.
    Image { image: Image },
    /// Reasoning the model produced, kept only so it can be handed back.
    ///
    /// Carries no text: what a human reads rides `AiEvent::ReasoningDelta`
    /// and the journal, and the two are not the same value — a block sent
    /// with `display: "omitted"`, and every `redacted_thinking` block, has
    /// state and no readable text at all.
    ///
    /// A block rather than a field beside the content for the reason this
    /// type's own doc gives one line up: reasoning state is *positional*.
    /// Anthropic requires a turn's thinking to precede its `tool_use`, and
    /// documents a hard `400` when the blocks are not replayed exactly,
    /// "including blocks whose `thinking` field is empty". A second carrier
    /// beside the ordered list would be two places for one fact.
    Reasoning { state: ReasoningState },
    /// The assistant asked for a tool to be run.
    ToolUse {
        /// The correlation token **of the protocol that produced this call**.
        ///
        /// Opaque and unguessable where the provider issues one, because the
        /// provider validates the id it issued — so a locally minted
        /// substitute is a rejected request. Where the provider issues none
        /// (Ollama's `api.ToolCall.ID` is `json:"id,omitempty"` and is
        /// populated by per-model parsers; Gemini's legacy `FunctionCall.id`
        /// is optional), the mapping mints one, and may do so **only because
        /// it is also the code that will send it back**.
        ///
        /// Both of those are properties of a protocol, not of this field, and
        /// stating one of them here as though it were the port's was the
        /// original error. What the port requires is only that the token
        /// exists: it is the key
        /// [`Conversation`]'s three relational invariants are defined over.
        call_id: String,
        name: String,
        /// The arguments, JSON or not.
        ///
        /// A `String` and not a `serde_json::Value` on purpose: a tool call
        /// truncated mid-stream arrives as invalid JSON, and that is
        /// *evidence about the stream*. Parsing here would discard it at the
        /// one seam that can still see it.
        ///
        /// "The verbatim bytes the model produced" is a property of protocols
        /// that transmit **string fragments**, not of this field — Gemini
        /// sends `functionCall.args` as a JSON object, and its mapping
        /// serialises it, which loses the original textual representation and
        /// not the value. Same correction as `call_id` above, same original
        /// mistake: one protocol's guarantee documented as the port's.
        args: String,
    },
    /// A tool's answer to one call.
    ToolResult {
        /// The `ToolUse::call_id` this answers.
        call_id: String,
        output: String,
        /// Whether the tool failed. Not the same as "no answer": a failure
        /// still answers the call, and a call that was never executed at all
        /// must still be answered this way — Anthropic requires a
        /// `tool_result` "marked with `is_error` set to true" for it.
        is_error: bool,
    },
}

/// One turn of conversation history sent to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// A message whose whole content is one block of text — every system
    /// prompt, every user turn that is not answering a tool.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

/// Why a list of messages is not a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryError {
    /// Images are input, never system instructions or model output.
    ImageFromNonUser,
    /// Durable image metadata exists but no session blob port was supplied.
    ImageBlobStoreRequired { reference: String },
    /// A referenced blob was absent, corrupt, or invalid as the recorded image.
    ImageUnavailable { reference: String, cause: String },
    /// Durable image metadata exceeds a prompt-wide product bound.
    ImagePromptInvalid { cause: String },
    /// A result names a call that no earlier message made.
    ResultWithoutCall { call_id: String },
    /// Two results name one call.
    CallAnsweredTwice { call_id: String },
    /// Two calls share one id, so no result can say which it answers.
    DuplicateCallId { call_id: String },
    /// A call was made and never answered. Every provider rejects this.
    CallUnanswered { call_id: String },
    /// A call appears in a turn the assistant did not author. Only the model
    /// asks for a tool to run, on every wire there is.
    CallFromNonAssistant { call_id: String },
    /// A result appears in an assistant turn. The assistant asks; something
    /// else answers — chat-completions gives the answer its own `tool` role
    /// and anthropic-messages puts it in a `user` turn, and neither lets the
    /// assistant answer itself.
    ResultFromAssistant { call_id: String },
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistoryError::ImageFromNonUser => write!(f, "only user messages may contain images"),
            HistoryError::ImageBlobStoreRequired { reference } => write!(
                f,
                "image blob {reference} requires the session blob store for replay"
            ),
            HistoryError::ImageUnavailable { reference, cause } => {
                write!(f, "image blob {reference} cannot be replayed: {cause}")
            }
            HistoryError::ImagePromptInvalid { cause } => {
                write!(f, "durable image prompt cannot be replayed: {cause}")
            }
            HistoryError::ResultWithoutCall { call_id } => {
                write!(f, "a result answers {call_id}, which nothing called")
            }
            HistoryError::CallAnsweredTwice { call_id } => {
                write!(f, "{call_id} was answered twice")
            }
            HistoryError::DuplicateCallId { call_id } => {
                write!(f, "two calls share the id {call_id}")
            }
            HistoryError::CallUnanswered { call_id } => {
                write!(f, "{call_id} was called and never answered")
            }
            HistoryError::CallFromNonAssistant { call_id } => {
                write!(
                    f,
                    "{call_id} was called by a turn the assistant did not author"
                )
            }
            HistoryError::ResultFromAssistant { call_id } => {
                write!(f, "the assistant cannot answer its own call {call_id}")
            }
        }
    }
}

impl std::error::Error for HistoryError {}

/// A message history that every wire format can express.
///
/// Holds the three relational invariants the providers agree on — a result
/// answers a call that came earlier, exactly once, and no call is left
/// hanging — and nothing else. Rules only one provider has (Anthropic's
/// grouping and block ordering, chat-completions' encodability) stay in the
/// wire formats, which keep refusing defensively: the two disagree about what
/// is valid, so a check here would have to pick one and be wrong for the
/// other.
///
/// It exists as a type rather than a convention because the invalid history
/// is not hypothetical. A session killed mid-turn resumes holding a call with
/// no result; with a bare `Vec` the loop can build that and learn about it
/// from a provider's 400, and with this it has to decide first — which is
/// what [`crate::generator::history`] does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Message>", into = "Vec<Message>")]
pub struct Conversation(Vec<Message>);

impl Conversation {
    /// Check `messages` and take ownership of them, or say what is wrong.
    pub fn new(messages: Vec<Message>) -> Result<Self, HistoryError> {
        // `false` until the call has been answered. A `BTreeMap` and not a
        // `HashMap`: the unanswered-call error names *a* call, and which one
        // it names should not depend on hash order.
        let mut calls: BTreeMap<&str, bool> = BTreeMap::new();
        let blocks = messages.iter().flat_map(|message| {
            message
                .content
                .iter()
                .map(move |block| (message.role, block))
        });
        for (role, block) in blocks {
            match block {
                // Neither takes part in the call/result relation these
                // invariants are about. Reasoning is ordered, and the block
                // list is what carries that; nothing here needs to know it.
                ContentBlock::Text { .. } | ContentBlock::Reasoning { .. } => {}
                ContentBlock::Image { .. } if role != Role::User => {
                    return Err(HistoryError::ImageFromNonUser);
                }
                ContentBlock::Image { .. } => {}
                ContentBlock::ToolUse { call_id, .. } => {
                    if role != Role::Assistant {
                        return Err(HistoryError::CallFromNonAssistant {
                            call_id: call_id.clone(),
                        });
                    }
                    if calls.insert(call_id, false).is_some() {
                        return Err(HistoryError::DuplicateCallId {
                            call_id: call_id.clone(),
                        });
                    }
                }
                ContentBlock::ToolResult { call_id, .. } if role == Role::Assistant => {
                    return Err(HistoryError::ResultFromAssistant {
                        call_id: call_id.clone(),
                    });
                }
                ContentBlock::ToolResult { call_id, .. } => match calls.get_mut(call_id.as_str()) {
                    // Walking in order is what makes this also catch a result
                    // that precedes its call: the call is not in the map yet.
                    None => {
                        return Err(HistoryError::ResultWithoutCall {
                            call_id: call_id.clone(),
                        });
                    }
                    Some(true) => {
                        return Err(HistoryError::CallAnsweredTwice {
                            call_id: call_id.clone(),
                        });
                    }
                    Some(answered) => *answered = true,
                },
            }
        }
        if let Some((call_id, _)) = calls.iter().find(|(_, answered)| !**answered) {
            return Err(HistoryError::CallUnanswered {
                call_id: (*call_id).to_string(),
            });
        }
        Ok(Conversation(messages))
    }

    /// The messages, in order.
    pub fn messages(&self) -> &[Message] {
        &self.0
    }
}

impl TryFrom<Vec<Message>> for Conversation {
    type Error = HistoryError;

    fn try_from(messages: Vec<Message>) -> Result<Self, Self::Error> {
        Conversation::new(messages)
    }
}

impl From<Conversation> for Vec<Message> {
    fn from(conversation: Conversation) -> Self {
        conversation.0
    }
}

/// A tool the model may call: name, description and JSON Schema for its
/// parameters — the same shape `Tool::descriptor` returns (ARCH §3.2), so a
/// `GenRequest` can be built directly from a tool registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Opaque, provider-private reasoning-continuation state (e.g. an encrypted
/// reasoning signature). `ego-core` never interprets the contents — only
/// whether a target `ModelCapabilities` declares it can carry this over
/// (`capabilities::ReasoningCarryover`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningState {
    /// Opaque, provider-private. Never parsed, never modified, never
    /// reordered — three separate documented `400`s.
    pub blob: String,
    /// Which dialect minted it, filled by the mapping that decoded it.
    ///
    /// Data, not a branch: compared for equality against the target model's
    /// own tag, never matched on. It exists because
    /// [`ReasoningCarryover::SameProviderOnly`](super::capabilities::ReasoningCarryover::SameProviderOnly)
    /// is a property of the *target* and says nothing about who minted the
    /// state — so without a tag, Anthropic state passes the check for a
    /// Gemini target and gets sent.
    pub dialect: String,
}

/// A request to generate the next assistant turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenRequest {
    /// Transient provider-cache policy and prefix boundary; never journaled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<PromptCache>,
    /// The model to generate with, as a provider-catalogue id (e.g.
    /// `"claude-opus-5"`). `ego-core` treats this as an opaque string; the
    /// catalogue and provider table live in `ego-ai` (ARCH §3.1).
    pub model: String,
    /// The history, already checked. A `Conversation` and not a
    /// `Vec<Message>` so that an unvalidated history cannot reach a
    /// [`Generator`](super::port::Generator) at all.
    ///
    /// Reasoning state carried over from a prior generation rides here too,
    /// as [`ContentBlock::Reasoning`]. It used to be a second field beside
    /// this one — the same fact in two places, and an input nothing in the
    /// tree could produce.
    pub messages: Conversation,
    pub tools: Vec<ToolDescriptor>,
    pub thinking: ThinkingLevel,
    pub max_output_tokens: Option<u64>,
    pub parallel_tool_calls: bool,
}

/// Stable messages precede the volatile context/current user request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCache {
    pub lineage_root: crate::event::id::SessionId,
    pub retention: super::capabilities::CacheRetention,
    pub stable_messages: usize,
}
