//! `ModelCapabilities`: the versioned record ARCH §3.1 and I5
//! (the design notes) require before treating a shared HTTP shape
//! as equivalent conversation semantics.
//!
//! A provider row is configuration, not proof two models behave the same in
//! conversation. `ModelCapabilities` is resolved per generation and checked
//! before a fallback: `fallback_gaps` reports every incompatibility rather
//! than letting a switch proceed and silently drop or mistranslate state.

use std::collections::BTreeSet;
use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};

use super::types::{
    ContentBlock, GenRequest, Image, ImageMediaType, MAX_IMAGE_BYTES, MAX_IMAGE_EDGE,
    MAX_IMAGE_PIXELS, MAX_IMAGES_PER_PROMPT, MAX_TOTAL_IMAGE_BYTES, Message, ReasoningState,
    ThinkingLevel,
};

/// Proven audio input contract; missing facts are never support.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioInputSupport {
    #[default]
    Unknown,
    Unsupported,
    /// The admitted PCM16 WAV subset on the public Chat Completions endpoint.
    SupportedWav,
}

/// Every image `messages` carries, in order.
fn images_in(messages: &[Message]) -> Vec<&Image> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Image { image } => Some(image),
            _ => None,
        })
        .collect()
}

/// Project external tool evidence for one captured model, without changing the
/// canonical history. Native attachments reserve their budget and remain strict.
/// Walking backwards admits the newest fitting images; replacing in place keeps
/// the surviving images and their host-authored source labels in display order.
pub(crate) fn project_tool_images(
    conversation: super::types::Conversation,
    capabilities: &ModelCapabilities,
) -> super::types::Conversation {
    use super::types::Conversation;
    let mut messages = Vec::<Message>::from(conversation);
    let native = images_in(&messages)
        .into_iter()
        .filter(|image| !image.is_tool_result())
        .collect::<Vec<_>>();
    let mut count = native.len();
    let mut bytes = native.iter().fold(0_u64, |sum, image| {
        sum.saturating_add(image.bytes().len() as u64)
    });
    for block in messages
        .iter_mut()
        .rev()
        .flat_map(|message| message.content.iter_mut().rev())
    {
        let ContentBlock::Image { image } = block else {
            continue;
        };
        if !image.is_tool_result() {
            continue;
        }
        let receipt = match &capabilities.image_input {
            ImageInputSupport::Unknown | ImageInputSupport::Unsupported => {
                Some("[Tool image unavailable: this model has no supported image input.]")
            }
            ImageInputSupport::Supported(limits) => {
                let max_count = limits
                    .max_images
                    .map_or(MAX_IMAGES_PER_PROMPT as u32, NonZeroU32::get)
                    as usize;
                let max_bytes = limits
                    .max_total_bytes
                    .map_or(MAX_TOTAL_IMAGE_BYTES, NonZeroU64::get);
                let image_bytes = image.bytes().len() as u64;
                if !limits.image_gaps(image, 1).is_empty() {
                    Some(
                        "[Tool image unavailable: its format or per-image limits are not supported by this model.]",
                    )
                } else if count >= max_count || bytes.saturating_add(image_bytes) > max_bytes {
                    Some(
                        "[Tool image unavailable: this request's image budget is reserved for native attachments and newer fitting tool images.]",
                    )
                } else {
                    count += 1;
                    bytes = bytes.saturating_add(image_bytes);
                    None
                }
            }
        };
        if let Some(text) = receipt {
            *block = ContentBlock::Text { text: text.into() };
        }
    }
    Conversation::new(messages).expect("replacing tool images changes no call/result invariant")
}

/// Per-model image limits, always bounded by the product's absolute limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageLimits {
    accepted_media_types: BTreeSet<ImageMediaType>,
    max_images: Option<NonZeroU32>,
    max_bytes_per_image: Option<NonZeroU64>,
    max_total_bytes: Option<NonZeroU64>,
    max_width: Option<NonZeroU32>,
    max_height: Option<NonZeroU32>,
    max_pixels: Option<NonZeroU64>,
}

impl ImageLimits {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        accepted_media_types: BTreeSet<ImageMediaType>,
        max_images: Option<NonZeroU32>,
        max_bytes_per_image: Option<NonZeroU64>,
        max_total_bytes: Option<NonZeroU64>,
        max_width: Option<NonZeroU32>,
        max_height: Option<NonZeroU32>,
        max_pixels: Option<NonZeroU64>,
    ) -> Result<Self, String> {
        if accepted_media_types.is_empty() {
            return Err("supported image input must name at least one media type".to_string());
        }
        validate_limit(
            max_images.map(|value| u64::from(value.get())),
            MAX_IMAGES_PER_PROMPT as u64,
            "max_images",
        )?;
        validate_limit(
            max_bytes_per_image.map(NonZeroU64::get),
            MAX_IMAGE_BYTES,
            "max_bytes_per_image",
        )?;
        validate_limit(
            max_total_bytes.map(NonZeroU64::get),
            MAX_TOTAL_IMAGE_BYTES,
            "max_total_bytes",
        )?;
        validate_limit(
            max_width.map(|value| u64::from(value.get())),
            u64::from(MAX_IMAGE_EDGE),
            "max_width",
        )?;
        validate_limit(
            max_height.map(|value| u64::from(value.get())),
            u64::from(MAX_IMAGE_EDGE),
            "max_height",
        )?;
        validate_limit(
            max_pixels.map(NonZeroU64::get),
            MAX_IMAGE_PIXELS,
            "max_pixels",
        )?;
        Ok(Self {
            accepted_media_types,
            max_images,
            max_bytes_per_image,
            max_total_bytes,
            max_width,
            max_height,
            max_pixels,
        })
    }

    /// The existing strict per-image policy, shared with transient tool projection.
    fn image_gaps(&self, image: &Image, index: usize) -> Vec<CapabilityGap> {
        let mut gaps = Vec::new();
        if !self.accepted_media_types.contains(&image.media_type()) {
            gaps.push(CapabilityGap::ImageMediaTypeUnsupported {
                index,
                media_type: image.media_type(),
            });
        }
        let bytes = image.bytes().len() as u64;
        let max = self
            .max_bytes_per_image
            .map_or(MAX_IMAGE_BYTES, NonZeroU64::get);
        if bytes > max {
            gaps.push(CapabilityGap::ImageTooLarge { index, bytes, max });
        }
        let max = self.max_width.map_or(MAX_IMAGE_EDGE, NonZeroU32::get);
        if image.width() > max {
            gaps.push(CapabilityGap::ImageTooWide {
                index,
                width: image.width(),
                max,
            });
        }
        let max = self.max_height.map_or(MAX_IMAGE_EDGE, NonZeroU32::get);
        if image.height() > max {
            gaps.push(CapabilityGap::ImageTooHigh {
                index,
                height: image.height(),
                max,
            });
        }
        let pixels = u64::from(image.width()).saturating_mul(u64::from(image.height()));
        let max = self.max_pixels.map_or(MAX_IMAGE_PIXELS, NonZeroU64::get);
        if pixels > max {
            gaps.push(CapabilityGap::ImageTooManyPixels { index, pixels, max });
        }
        gaps
    }

    pub fn accepted_media_types(&self) -> &BTreeSet<ImageMediaType> {
        &self.accepted_media_types
    }
    pub const fn max_images(&self) -> Option<NonZeroU32> {
        self.max_images
    }
    pub const fn max_bytes_per_image(&self) -> Option<NonZeroU64> {
        self.max_bytes_per_image
    }
    pub const fn max_total_bytes(&self) -> Option<NonZeroU64> {
        self.max_total_bytes
    }
    pub const fn max_width(&self) -> Option<NonZeroU32> {
        self.max_width
    }
    pub const fn max_height(&self) -> Option<NonZeroU32> {
        self.max_height
    }
    pub const fn max_pixels(&self) -> Option<NonZeroU64> {
        self.max_pixels
    }
}

fn validate_limit(value: Option<u64>, product: u64, name: &str) -> Result<(), String> {
    if value.is_some_and(|value| value > product) {
        return Err(format!("{name} cannot exceed the product limit {product}"));
    }
    Ok(())
}

/// Whether this exact model accepts image input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ImageInputSupport {
    #[default]
    Unknown,
    Unsupported,
    Supported(ImageLimits),
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ImageInputStateWire {
    Unknown,
    Unsupported,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SupportedImageInputWire {
    kind: SupportedImageKind,
    accepted_media_types: BTreeSet<ImageMediaType>,
    max_images: Option<NonZeroU32>,
    max_bytes_per_image: Option<NonZeroU64>,
    max_total_bytes: Option<NonZeroU64>,
    max_width: Option<NonZeroU32>,
    max_height: Option<NonZeroU32>,
    max_pixels: Option<NonZeroU64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum SupportedImageKind {
    Supported,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ImageInputWire {
    State(ImageInputStateWire),
    Supported(SupportedImageInputWire),
}

impl<'de> Deserialize<'de> for ImageInputSupport {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match ImageInputWire::deserialize(deserializer)? {
            ImageInputWire::State(ImageInputStateWire::Unknown) => Ok(Self::Unknown),
            ImageInputWire::State(ImageInputStateWire::Unsupported) => Ok(Self::Unsupported),
            ImageInputWire::Supported(wire) => {
                let SupportedImageInputWire {
                    kind: SupportedImageKind::Supported,
                    accepted_media_types,
                    max_images,
                    max_bytes_per_image,
                    max_total_bytes,
                    max_width,
                    max_height,
                    max_pixels,
                } = wire;
                ImageLimits::new(
                    accepted_media_types,
                    max_images,
                    max_bytes_per_image,
                    max_total_bytes,
                    max_width,
                    max_height,
                    max_pixels,
                )
                .map(Self::Supported)
                .map_err(serde::de::Error::custom)
            }
        }
    }
}

impl Serialize for ImageInputSupport {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Unknown => serializer.serialize_str("unknown"),
            Self::Unsupported => serializer.serialize_str("unsupported"),
            Self::Supported(limits) => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("kind", "supported")?;
                map.serialize_entry("accepted_media_types", &limits.accepted_media_types)?;
                if let Some(value) = limits.max_images {
                    map.serialize_entry("max_images", &value)?;
                }
                if let Some(value) = limits.max_bytes_per_image {
                    map.serialize_entry("max_bytes_per_image", &value)?;
                }
                if let Some(value) = limits.max_total_bytes {
                    map.serialize_entry("max_total_bytes", &value)?;
                }
                if let Some(value) = limits.max_width {
                    map.serialize_entry("max_width", &value)?;
                }
                if let Some(value) = limits.max_height {
                    map.serialize_entry("max_height", &value)?;
                }
                if let Some(value) = limits.max_pixels {
                    map.serialize_entry("max_pixels", &value)?;
                }
                map.end()
            }
        }
    }
}

/// Every reasoning state `req`'s history carries, in the order it carries
/// them.
fn carried_reasoning(req: &GenRequest) -> Vec<&ReasoningState> {
    req.messages
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Reasoning { state } => Some(state),
            _ => None,
        })
        .collect()
}

/// Whether a model can accept reasoning state carried over from another
/// generation (its own prior turn, or — for a fallback — a different model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningCarryover {
    /// No reasoning-state carryover accepted at all.
    Unsupported,
    /// Only understands its own provider's reasoning state.
    SameProviderOnly,
    /// Accepts reasoning state produced by any provider.
    Portable,
}

/// How a generation can be stopped mid-stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelSemantics {
    /// The generation cannot be cancelled once started.
    Unsupported,
    /// The provider stops at the next safe boundary (e.g. token boundary).
    Cooperative,
    /// The provider stops immediately, mid-token if necessary.
    Immediate,
}

/// Prompt/response cache retention, pi's `CacheRetention` (the design notes
/// §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    None,
    Short,
    Long,
}

/// Which [`ThinkingLevel`]s a model accepts on the wire.
///
/// Three states rather than an `Option`, decided in
/// the design notes §4 after Codex refused
/// the `Option` this repository first proposed. `None` would have had to mean
/// "nothing stated, so send anything", and an absent field read as a
/// permissive fact is the mistake
/// `2026-08-21-what-a-model-resolves-to.md` §4 already refused twice: it is
/// `/api/show`'s architecture context read as a served ceiling, and `:cloud`'s
/// `auth: None` read as "not billed". A model that states nothing about
/// `reasoning_effort` is not a model that accepts every level; it is a model
/// nobody asked.
///
/// The distinction is load-bearing because the parameter is not advisory.
/// `reasoning_effort` is always emitted — omitting it selects the schema's
/// documented default of `medium`, which would bill a caller for reasoning
/// they switched off — and an unrecognised value is a hard `400`, not an
/// ignore. xAI accepts a different subset per model; Groq accepts only two
/// values; some rows reject the parameter outright.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffortSupport {
    /// Nobody stated it. Refuse until this model is resolved again.
    ///
    /// What a [`ModelCapabilities`] persisted before this field existed
    /// decodes as, because that is what it proved — hence [`Default`], which
    /// is what `serde` reaches for when the field is absent.
    #[default]
    Unknown,
    /// Measured, not assumed: every level [`ThinkingLevel`] can produce is
    /// accepted. A claim a row may only make from something it observed.
    Any,
    /// Exactly these. Anything else is refused naming both sides.
    Only(BTreeSet<ThinkingLevel>),
}

/// The wire transport a generation streams over.
///
/// Hosted rows can use SSE, WebSocket or native Amazon eventstream framing.
/// Automatic selection is policy, not a transport, and unsupported spellings
/// must fail during decoding.
///
/// **A variant is added here when a row produces it** — an implemented
/// transport, a decoder that frames it, and a generation that resolves to it.
/// A vendor documenting one as optional beside an SSE this build already
/// speaks is not that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Sse,
    WebSocket,
    /// Native binary streaming; a tool turn can use synchronous JSON over HTTP.
    AmazonEventStream,
}

/// A versioned snapshot of what a model can do in conversation, resolved per
/// generation and persisted in the run manifest (ARCH §3.1, I5). Every field
/// here is one the I5 gap explicitly names: tool/schema limits, parallel
/// tool calls, reasoning-state carryover, context/output ceilings, cancel
/// semantics, cache behaviour, transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilities {
    /// Bumped whenever the field set or its meaning changes, so a persisted
    /// record can be told apart from a stale shape.
    pub schema_version: u32,
    pub max_context_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// Tool/schema limit: the most tools this model accepts in one request.
    pub max_tools: Option<u32>,
    pub supports_parallel_tool_calls: bool,
    pub reasoning_carryover: ReasoningCarryover,
    /// The reasoning efforts this model accepts. Supplied for hosted models,
    /// discovered for local ones, and [`Unknown`] for anything that predates
    /// the field — see [`ReasoningEffortSupport`].
    ///
    /// [`Unknown`]: ReasoningEffortSupport::Unknown
    #[serde(default)]
    pub reasoning_efforts: ReasoningEffortSupport,
    /// The effort selected when no caller, configuration layer, or host
    /// supplied one. `None` means the current fact source did not establish a
    /// default; it never means `off` or the first accepted level.
    #[serde(default)]
    pub default_reasoning_effort: Option<ThinkingLevel>,
    /// Which reasoning-state dialect this model mints and accepts, when
    /// anybody said.
    ///
    /// Compared for equality against [`ReasoningState::dialect`], never
    /// matched on: this is the target's half of a check whose other half
    /// travels with the state. It matters only under
    /// [`ReasoningCarryover::SameProviderOnly`], which constrains *whose*
    /// state a model accepts while saying nothing about who minted the state
    /// in hand — so without both tags the check cannot be made at all, and
    /// Anthropic state passes for a Gemini target.
    ///
    /// `None` is "nobody stated one", not "any dialect". A record written
    /// before this field existed decodes that way and has proved nothing
    /// about the model, exactly as a `1` record proves nothing about
    /// [`reasoning_efforts`](ModelCapabilities::reasoning_efforts).
    ///
    /// [`ReasoningState::dialect`]: super::types::ReasoningState::dialect
    #[serde(default)]
    pub reasoning_state_dialect: Option<String>,
    pub cancel: CancelSemantics,
    pub cache: CacheRetention,
    pub transport: Transport,
    /// Exact image-input facts. Missing older fields fail closed.
    #[serde(default)]
    pub image_input: ImageInputSupport,
    #[serde(default)]
    pub audio_input: AudioInputSupport,
}

/// One way a `GenRequest` does not fit a model's capabilities.
///
/// Originally written for a fallback — "would this request still fit if we
/// switched models" — which turned out to be the narrower reading of the same
/// sentence. Every variant is equally a statement about a *first* attempt, so
/// [`request_gaps`](ModelCapabilities::request_gaps) asks it of one model and
/// [`fallback_gaps`](ModelCapabilities::fallback_gaps) asks it of the model
/// being switched to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilityGap {
    AudioInputUnknown,
    AudioInputUnsupported,
    ImageInputUnknown,
    /// Where the images came from, because the gap is a statement about the
    /// whole request and the person only sent the last message of it.
    ///
    /// `supplied` counts the images in the newest prompt and `carried` counts
    /// the ones already in the conversation before it. They are never both
    /// zero: the gap is only produced when the request carries an image.
    ///
    /// Without the split this refused a text-only prompt with "unsupported
    /// image input", which is true of the request and a lie to the person —
    /// reachable by attaching an image and then changing model mid-session.
    ImageInputUnsupported {
        supplied: usize,
        carried: usize,
    },
    ImageMediaTypeUnsupported {
        index: usize,
        media_type: ImageMediaType,
    },
    TooManyImages {
        requested: usize,
        max: u32,
    },
    ImageTooLarge {
        index: usize,
        bytes: u64,
        max: u64,
    },
    TotalImageBytesTooLarge {
        requested: u64,
        max: u64,
    },
    ImageTooWide {
        index: usize,
        width: u32,
        max: u32,
    },
    ImageTooHigh {
        index: usize,
        height: u32,
        max: u32,
    },
    ImageTooManyPixels {
        index: usize,
        pixels: u64,
        max: u64,
    },
    /// `req` carries reasoning state the model can't accept.
    ReasoningStateCarryoverUnsupported,
    /// `req` requires parallel tool calls the model doesn't support.
    ParallelToolCallsUnsupported,
    /// `req` offers more tools than the model accepts in one request.
    TooManyTools {
        requested: usize,
        max: u32,
    },
    /// `req` asks for more output tokens than the model allows.
    OutputTooLarge {
        requested: u64,
        max: u64,
    },
    /// Nobody ever stated which reasoning efforts this model accepts, so
    /// sending one would be a guess. Distinct from
    /// [`ReasoningEffortUnsupported`] because the answer is "re-resolve this
    /// model", not "ask for a different level".
    ///
    /// [`ReasoningEffortUnsupported`]: CapabilityGap::ReasoningEffortUnsupported
    ReasoningEffortUnstated {
        requested: ThinkingLevel,
    },
    /// The model states what it accepts, and `req` asked for something else.
    /// Both sides travel, so the refusal can name the levels that would work.
    ReasoningEffortUnsupported {
        requested: ThinkingLevel,
        accepted: BTreeSet<ThinkingLevel>,
    },
    /// `req` carries reasoning state minted by one dialect, and this model
    /// accepts only its own — a different one.
    ///
    /// Reached only under [`ReasoningCarryover::SameProviderOnly`]:
    /// `Unsupported` refuses any carried state before the dialects are
    /// consulted, and `Portable` accepts every dialect by definition.
    ReasoningStateDialectMismatch {
        carried: String,
        accepted: String,
    },
    /// `req` carries reasoning state and nobody ever stated which dialect
    /// this model speaks, so whether the state is replayable is unknown.
    ///
    /// Separate from [`ReasoningStateDialectMismatch`] on the same grounds
    /// that separate [`ReasoningEffortUnstated`] from
    /// [`ReasoningEffortUnsupported`]: the remedy is "re-resolve this model",
    /// not "carry different state". Failing closed here rather than assuming
    /// a match, because a wrong guess sends state a provider documents a
    /// `400` for.
    ///
    /// [`ReasoningStateDialectMismatch`]: CapabilityGap::ReasoningStateDialectMismatch
    /// [`ReasoningEffortUnstated`]: CapabilityGap::ReasoningEffortUnstated
    /// [`ReasoningEffortUnsupported`]: CapabilityGap::ReasoningEffortUnsupported
    ReasoningStateDialectUnstated {
        carried: String,
    },
}

/// Why an optional reasoning selection could not become one concrete effort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningSelectionError {
    UnsupportedCapabilitiesVersion {
        found: u32,
        required: u32,
    },
    DefaultUnstated,
    EffortUnstated {
        requested: ThinkingLevel,
    },
    EffortUnsupported {
        requested: ThinkingLevel,
        accepted: BTreeSet<ThinkingLevel>,
    },
}

impl fmt::Display for ReasoningSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedCapabilitiesVersion { found, required } => write!(
                f,
                "unsupported model capabilities version {found}; version {required} is required"
            ),
            Self::DefaultUnstated => f.write_str(
                "the model does not state a default reasoning effort; pass --thinking \
                 off|minimal|low|medium|high|xhigh|max",
            ),
            Self::EffortUnstated { requested } => write!(
                f,
                "nobody stated whether the model accepts `{}` reasoning",
                requested.as_str()
            ),
            Self::EffortUnsupported {
                requested,
                accepted,
            } => write!(
                f,
                "the model does not accept `{}` reasoning; accepted: {}",
                requested.as_str(),
                accepted
                    .iter()
                    .map(|level| level.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::error::Error for ReasoningSelectionError {}

/// What the caller is told. Every gap names both what was asked for and what
/// would have worked, because a usage error the caller has to guess at is only
/// marginally better than no message.
impl std::fmt::Display for CapabilityGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AudioInputUnknown => {
                f.write_str("unknown audio input support; audio was not sent")
            }
            Self::AudioInputUnsupported => f.write_str(
                "this model cannot accept audio; select an audio-capable model; audio was not sent",
            ),
            Self::ImageInputUnknown => {
                write!(f, "unknown image input support; resolve the model again")
            }
            // Three sentences rather than one, because the remedy differs:
            // a prompt that supplied an image can drop it, and a conversation
            // that already carries one cannot — the history is resent whole
            // on every turn, so the only ways out are a different model or a
            // different session.
            Self::ImageInputUnsupported { supplied, carried } => match (supplied, carried) {
                (0, carried) => write!(
                    f,
                    "this model does not accept images, and this conversation \
                     already carries {carried} from earlier in it; the history \
                     is sent whole on every turn, so switch back to a model \
                     that accepts images or start a new session"
                ),
                (supplied, 0) => write!(
                    f,
                    "this model does not accept images, and this prompt \
                     supplied {supplied}"
                ),
                (supplied, carried) => write!(
                    f,
                    "this model does not accept images: this prompt supplied \
                     {supplied}, and this conversation already carries \
                     {carried} from earlier in it that are sent with it"
                ),
            },
            Self::ImageMediaTypeUnsupported { index, media_type } => write!(
                f,
                "image {index} is {}, which this model does not accept",
                media_type.mime()
            ),
            Self::TooManyImages { requested, max } => write!(
                f,
                "{requested} images supplied, but at most {max} are accepted"
            ),
            Self::ImageTooLarge { index, bytes, max } => write!(
                f,
                "image {index} has {bytes} bytes, above the {max}-byte limit"
            ),
            Self::TotalImageBytesTooLarge { requested, max } => write!(
                f,
                "total image input has {requested} bytes, above the {max}-byte limit"
            ),
            Self::ImageTooWide { index, width, max } => {
                write!(f, "image {index} width {width} exceeds {max}")
            }
            Self::ImageTooHigh { index, height, max } => {
                write!(f, "image {index} height {height} exceeds {max}")
            }
            Self::ImageTooManyPixels { index, pixels, max } => {
                write!(f, "image {index} has {pixels} pixels, above {max}")
            }
            Self::ReasoningStateCarryoverUnsupported => {
                write!(
                    f,
                    "this model does not accept reasoning state carried over from another generation"
                )
            }
            Self::ParallelToolCallsUnsupported => {
                write!(f, "this model does not support parallel tool calls")
            }
            Self::TooManyTools { requested, max } => {
                write!(
                    f,
                    "{requested} tools offered, this model accepts at most {max}"
                )
            }
            Self::OutputTooLarge { requested, max } => {
                write!(
                    f,
                    "{requested} output tokens requested, this model allows at most {max}"
                )
            }
            Self::ReasoningEffortUnstated { requested } => write!(
                f,
                "reasoning effort {requested:?} was requested, but nobody ever stated which \
                 efforts this model accepts — resolve the model again"
            ),
            Self::ReasoningEffortUnsupported {
                requested,
                accepted,
            } => {
                let accepted: Vec<_> = accepted.iter().map(|level| format!("{level:?}")).collect();
                write!(
                    f,
                    "reasoning effort {requested:?} is not accepted by this model; it accepts {}",
                    accepted.join(", ")
                )
            }
            Self::ReasoningStateDialectMismatch { carried, accepted } => write!(
                f,
                "the history carries {carried} reasoning state and this model accepts \
                 only its own {accepted} state"
            ),
            Self::ReasoningStateDialectUnstated { carried } => write!(
                f,
                "the history carries {carried} reasoning state and nobody ever stated \
                 which dialect this model accepts — resolve the model again"
            ),
        }
    }
}

impl ModelCapabilities {
    /// The exact version this build writes and admits. Structural decoding is
    /// deliberately separate from admission so a stale durable record can be
    /// identified and refused by its typed boundary rather than misreported
    /// as corrupt data.
    ///
    /// [`reasoning_efforts`]: ModelCapabilities::reasoning_efforts
    /// [`reasoning_state_dialect`]: ModelCapabilities::reasoning_state_dialect
    pub const SCHEMA_VERSION: u32 = 6;

    /// Resolve one explicit override or the model's stated default, then
    /// validate that exact value against the existing reasoning-support fact.
    pub fn resolve_reasoning_effort(
        &self,
        requested: Option<ThinkingLevel>,
    ) -> Result<ThinkingLevel, ReasoningSelectionError> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(ReasoningSelectionError::UnsupportedCapabilitiesVersion {
                found: self.schema_version,
                required: Self::SCHEMA_VERSION,
            });
        }
        let selected = requested
            .or(self.default_reasoning_effort)
            .ok_or(ReasoningSelectionError::DefaultUnstated)?;
        self.validate_reasoning_effort(selected)?;
        Ok(selected)
    }

    fn validate_reasoning_effort(
        &self,
        requested: ThinkingLevel,
    ) -> Result<(), ReasoningSelectionError> {
        match &self.reasoning_efforts {
            ReasoningEffortSupport::Unknown => {
                Err(ReasoningSelectionError::EffortUnstated { requested })
            }
            ReasoningEffortSupport::Any => Ok(()),
            ReasoningEffortSupport::Only(accepted) if accepted.contains(&requested) => Ok(()),
            ReasoningEffortSupport::Only(accepted) => {
                Err(ReasoningSelectionError::EffortUnsupported {
                    requested,
                    accepted: accepted.clone(),
                })
            }
        }
    }

    /// Whether this model accepts the opaque provider state of `block`
    /// replayed in its history; `true` for every block without such state.
    /// The one rule both [`request_gaps`](Self::request_gaps) and the turn's
    /// removal of foreign state apply, so the two cannot disagree.
    ///
    /// Provider content is accepted only in the dialect this model states:
    /// it is the provider's own output item, replayed verbatim, whatever
    /// [`ReasoningCarryover`] says about reasoning.
    pub fn accepts_block(&self, block: &ContentBlock) -> bool {
        match block {
            ContentBlock::Reasoning { state } => self.accepts_reasoning_state(state),
            ContentBlock::ProviderContent { state, .. } => {
                self.reasoning_state_dialect.as_deref() == Some(state.dialect.as_str())
            }
            _ => true,
        }
    }

    fn accepts_reasoning_state(&self, state: &ReasoningState) -> bool {
        match self.reasoning_carryover {
            ReasoningCarryover::Unsupported => false,
            ReasoningCarryover::SameProviderOnly => {
                self.reasoning_state_dialect.as_deref() == Some(state.dialect.as_str())
            }
            ReasoningCarryover::Portable => true,
        }
    }

    /// Every way `req` does not fit *this* model — empty means it can be
    /// sent. Checked after the model resolves and before the budget gate
    /// reserves against it: a reservation taken for a request the model will
    /// reject has authorised spending on something that was never going to be
    /// sent.
    pub fn request_gaps(&self, req: &GenRequest) -> Vec<CapabilityGap> {
        let mut gaps = Vec::new();
        if req
            .messages
            .messages()
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::Audio { .. }))
        {
            match self.audio_input {
                AudioInputSupport::Unknown => gaps.push(CapabilityGap::AudioInputUnknown),
                AudioInputSupport::Unsupported => gaps.push(CapabilityGap::AudioInputUnsupported),
                AudioInputSupport::SupportedWav => {}
            }
        }

        // Split at the newest message rather than counting the request as one
        // heap. Every gap below is a fact about the whole request — history is
        // part of what gets sent — but the *refusal* is read by somebody who
        // only wrote the last message, and one of these two numbers is the
        // difference between "drop the attachment" and "you cannot".
        let (earlier, newest) = req
            .messages
            .messages()
            .split_last()
            .map_or((&[][..], &[][..]), |(newest, earlier)| {
                (earlier, std::slice::from_ref(newest))
            });
        let carried = images_in(earlier);
        let supplied = images_in(newest);
        // Back in order, because the `index` every limit gap reports is a
        // position in the request as sent.
        let images: Vec<_> = carried.iter().chain(supplied.iter()).copied().collect();
        if !images.is_empty() {
            match &self.image_input {
                ImageInputSupport::Unknown => gaps.push(CapabilityGap::ImageInputUnknown),
                ImageInputSupport::Unsupported => gaps.push(CapabilityGap::ImageInputUnsupported {
                    supplied: supplied.len(),
                    carried: carried.len(),
                }),
                ImageInputSupport::Supported(limits) => {
                    // Preserve native gap ordering: formats, count, bytes, total,
                    // then dimensions. The same per-image policy projects tool evidence.
                    let per_image: Vec<_> = images
                        .iter()
                        .enumerate()
                        .flat_map(|(offset, image)| limits.image_gaps(image, offset + 1))
                        .collect();
                    gaps.extend(
                        per_image
                            .iter()
                            .filter(|gap| {
                                matches!(gap, CapabilityGap::ImageMediaTypeUnsupported { .. })
                            })
                            .cloned(),
                    );
                    let max_images = limits
                        .max_images
                        .map_or(MAX_IMAGES_PER_PROMPT as u32, NonZeroU32::get);
                    if images.len() > max_images as usize {
                        gaps.push(CapabilityGap::TooManyImages {
                            requested: images.len(),
                            max: max_images,
                        });
                    }
                    gaps.extend(
                        per_image
                            .iter()
                            .filter(|gap| matches!(gap, CapabilityGap::ImageTooLarge { .. }))
                            .cloned(),
                    );
                    let total = images.iter().fold(0_u64, |sum, image| {
                        sum.saturating_add(image.bytes().len() as u64)
                    });
                    let max_total = limits
                        .max_total_bytes
                        .map_or(MAX_TOTAL_IMAGE_BYTES, NonZeroU64::get);
                    if total > max_total {
                        gaps.push(CapabilityGap::TotalImageBytesTooLarge {
                            requested: total,
                            max: max_total,
                        });
                    }
                    gaps.extend(per_image.into_iter().filter(|gap| {
                        matches!(
                            gap,
                            CapabilityGap::ImageTooWide { .. }
                                | CapabilityGap::ImageTooHigh { .. }
                                | CapabilityGap::ImageTooManyPixels { .. }
                        )
                    }));
                }
            }
        }

        // Read off the conversation rather than a field beside it: the state
        // is positional, so the block list is the only place it can live, and
        // a second copy on the request would be the same fact twice.
        let carried: Vec<_> = carried_reasoning(req)
            .into_iter()
            .filter(|state| !self.accepts_reasoning_state(state))
            .collect();
        match self.reasoning_carryover {
            _ if carried.is_empty() => {}
            ReasoningCarryover::Unsupported => {
                gaps.push(CapabilityGap::ReasoningStateCarryoverUnsupported);
            }
            // Constrains *whose* state this model accepts, so the tags have
            // to be compared. Reported once per offending dialect and not
            // once per block: a history with forty blocks from one other
            // provider is one problem with one remedy.
            ReasoningCarryover::SameProviderOnly => {
                let mut reported = BTreeSet::new();
                for state in &carried {
                    match &self.reasoning_state_dialect {
                        _ if !reported.insert(state.dialect.clone()) => {}
                        Some(mine) => gaps.push(CapabilityGap::ReasoningStateDialectMismatch {
                            carried: state.dialect.clone(),
                            accepted: mine.clone(),
                        }),
                        None => gaps.push(CapabilityGap::ReasoningStateDialectUnstated {
                            carried: state.dialect.clone(),
                        }),
                    }
                }
            }
            ReasoningCarryover::Portable => {}
        }
        if req.parallel_tool_calls && !self.supports_parallel_tool_calls {
            gaps.push(CapabilityGap::ParallelToolCallsUnsupported);
        }
        if let Some(max) = self.max_tools {
            let requested = req.tools.len();
            if requested > max as usize {
                gaps.push(CapabilityGap::TooManyTools { requested, max });
            }
        }
        if let (Some(requested), Some(max)) = (req.max_output_tokens, self.max_output_tokens)
            && requested > max
        {
            gaps.push(CapabilityGap::OutputTooLarge { requested, max });
        }
        match self.validate_reasoning_effort(req.thinking) {
            Err(ReasoningSelectionError::EffortUnstated { requested }) => {
                gaps.push(CapabilityGap::ReasoningEffortUnstated { requested });
            }
            Err(ReasoningSelectionError::EffortUnsupported {
                requested,
                accepted,
            }) => {
                gaps.push(CapabilityGap::ReasoningEffortUnsupported {
                    requested,
                    accepted,
                });
            }
            Ok(())
            | Err(ReasoningSelectionError::UnsupportedCapabilitiesVersion { .. })
            | Err(ReasoningSelectionError::DefaultUnstated) => {}
        }

        gaps
    }

    /// Every way falling `req` back from a model with these capabilities to a
    /// model with `target`'s would break — empty means the fallback is safe.
    /// Never attempt a fallback without checking this first (ARCH §3.1): an
    /// incompatible switch must be detected and reported, not silently
    /// attempted.
    ///
    /// This is [`request_gaps`](Self::request_gaps) asked of the target, and
    /// nothing else. The question "does `req` fit `target`" does not depend on
    /// where `req` was going to go before — which is why `self` is unused, and
    /// why the two are one function rather than two that can drift apart. The
    /// name is kept because the *reason to ask* differs, and a call site that
    /// says `fallback_gaps` is saying which of the two it means.
    pub fn fallback_gaps(
        &self,
        target: &ModelCapabilities,
        req: &GenRequest,
    ) -> Vec<CapabilityGap> {
        target.request_gaps(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unsupported transports must fail when operator-supplied facts decode.
    #[test]
    fn a_transport_this_build_cannot_speak_does_not_decode() {
        for spelling in ["web_transport", "auto", "websocket", "grpc"] {
            let named = format!("\"{spelling}\"");

            assert!(
                serde_json::from_str::<Transport>(&named).is_err(),
                "`{spelling}` decoded as a transport this build cannot speak"
            );
        }
    }

    /// And the one it can still does, so the guard above cannot pass by
    /// refusing everything.
    #[test]
    fn the_transport_this_build_speaks_still_decodes() {
        assert_eq!(
            serde_json::from_str::<Transport>("\"web_socket\"").expect("WebSocket decodes"),
            Transport::WebSocket
        );
        assert_eq!(
            serde_json::from_str::<Transport>("\"sse\"").expect("sse decodes"),
            Transport::Sse
        );
    }
}

#[cfg(test)]
mod tool_image_projection_tests {
    use super::*;
    use crate::generator::types::{Conversation, Message, Role};
    const PNG_1X1: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4,
        0, 0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 100, 248, 15, 0, 1, 5,
        1, 1, 39, 24, 227, 102, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    const JPEG_1X1: &[u8] = &[
        255, 216, 255, 224, 0, 16, 74, 70, 73, 70, 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, 255, 219, 0, 67,
        0, 8, 6, 6, 7, 6, 5, 8, 7, 7, 7, 9, 9, 8, 10, 12, 20, 13, 12, 11, 11, 12, 25, 18, 19, 15,
        20, 29, 26, 31, 30, 29, 26, 28, 28, 32, 36, 46, 39, 32, 34, 44, 35, 28, 28, 40, 55, 41, 44,
        48, 49, 52, 52, 52, 31, 39, 57, 61, 56, 50, 60, 46, 51, 52, 50, 255, 192, 0, 17, 8, 0, 1,
        0, 1, 3, 1, 34, 0, 2, 17, 1, 3, 17, 1, 255, 196, 0, 20, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 255, 196, 0, 20, 16, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 255, 218, 0, 12, 3, 1, 0, 2, 17, 3, 17, 0, 63, 0, 255, 217,
    ];

    fn crc(bytes: &[u8]) -> u32 {
        let mut value = 0xffff_ffff_u32;
        for byte in bytes {
            value ^= u32::from(*byte);
            for _ in 0..8 {
                value = if value & 1 == 1 {
                    (value >> 1) ^ 0xedb8_8320
                } else {
                    value >> 1
                };
            }
        }
        !value
    }

    fn large_png(payload: usize) -> Vec<u8> {
        let mut bytes = PNG_1X1[..PNG_1X1.len() - 12].to_vec();
        let mut chunk = b"ruSt".to_vec();
        chunk.resize(chunk.len() + payload, b'x');
        bytes.extend_from_slice(&((chunk.len() - 4) as u32).to_be_bytes());
        bytes.extend_from_slice(&chunk);
        bytes.extend_from_slice(&crc(&chunk).to_be_bytes());
        bytes.extend_from_slice(&PNG_1X1[PNG_1X1.len() - 12..]);
        bytes
    }

    fn caps(image_input: ImageInputSupport) -> ModelCapabilities {
        let mut caps: ModelCapabilities = serde_json::from_value(serde_json::json!({
            "schema_version": ModelCapabilities::SCHEMA_VERSION,
            "max_context_tokens": null, "max_output_tokens": 1024,
            "max_tools": null, "supports_parallel_tool_calls": false,
            "reasoning_carryover": "unsupported", "reasoning_efforts": "any",
            "reasoning_state_dialect": null, "cancel": "cooperative", "cache": "none",
            "transport": "sse"
        }))
        .unwrap();
        caps.image_input = image_input;
        caps
    }
    fn limits(count: Option<u32>, per_image: Option<u64>, total: Option<u64>) -> ImageInputSupport {
        ImageInputSupport::Supported(
            ImageLimits::new(
                BTreeSet::from([ImageMediaType::Png]),
                count.and_then(NonZeroU32::new),
                per_image.and_then(NonZeroU64::new),
                total.and_then(NonZeroU64::new),
                None,
                None,
                None,
            )
            .unwrap(),
        )
    }
    fn image() -> Image {
        Image::new(PNG_1X1.to_vec(), ImageMediaType::Png).unwrap()
    }
    fn conversation(images: impl IntoIterator<Item = Image>) -> Conversation {
        Conversation::new(vec![Message {
            role: Role::User,
            content: images
                .into_iter()
                .map(|image| ContentBlock::Image { image })
                .collect(),
        }])
        .unwrap()
    }
    fn request(messages: Conversation) -> GenRequest {
        GenRequest {
            web_search: None,
            cache: None,
            model: "fixture".into(),
            messages,
            tools: vec![],
            thinking: ThinkingLevel::Off,
            max_output_tokens: Some(64),
            parallel_tool_calls: false,
        }
    }
    #[test]
    fn tool_origin_never_changes_image_or_provider_neutral_serialization() {
        // Catches accidental durable/wire provenance and clone tag loss.
        let native = image();
        let tool = native.clone().with_tool_result_origin();
        assert!(tool.clone().is_tool_result());
        assert_eq!(native.bytes(), tool.bytes());
        assert_eq!(
            serde_json::to_vec(&native).unwrap(),
            serde_json::to_vec(&tool).unwrap()
        );
        let roundtrip: Image = serde_json::from_slice(&serde_json::to_vec(&tool).unwrap()).unwrap();
        assert!(
            !roundtrip.is_tool_result(),
            "history must derive provenance again"
        );
        assert_eq!(
            serde_json::to_vec(&request(conversation([native]))).unwrap(),
            serde_json::to_vec(&request(conversation([tool]))).unwrap()
        );
    }
    #[test]
    fn newest_hundred_tool_images_keep_display_order_and_native_reserves_first() {
        // Catches first-100 truncation and letting tool images evict user input.
        let canonical =
            conversation((0..101).map(|n| image().with_text_offset(n).with_tool_result_origin()));
        let projected = project_tool_images(canonical.clone(), &caps(limits(None, None, None)));
        let kept = images_in(projected.messages());
        assert_eq!(kept.len(), 100);
        assert_eq!(
            kept.iter()
                .map(|i| i.text_offset().unwrap())
                .collect::<Vec<_>>(),
            (1..101).collect::<Vec<_>>()
        );
        assert_eq!(images_in(canonical.messages()).len(), 101);
        let native = image().with_text_offset(999);
        let projected = project_tool_images(
            conversation([
                image().with_tool_result_origin(),
                native.clone(),
                image().with_text_offset(2).with_tool_result_origin(),
            ]),
            &caps(limits(Some(2), None, None)),
        );
        let kept = images_in(projected.messages());
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0], &native);
        assert_eq!(kept[1].text_offset(), Some(2));
    }
    #[test]
    fn oversized_tool_image_does_not_poison_fitting_sibling_or_total_budget() {
        // Catches counting rejected bytes, dropping all siblings, and ignoring
        // the selected model's 10 MiB per-image ceiling.
        let huge = Image::new(large_png(10 * 1024 * 1024), ImageMediaType::Png)
            .unwrap()
            .with_tool_result_origin();
        let small = image().with_tool_result_origin();
        let caps = caps(limits(
            None,
            Some(10 * 1024 * 1024),
            Some(small.bytes().len() as u64),
        ));
        let projected = project_tool_images(conversation([small.clone(), huge]), &caps);
        assert_eq!(images_in(projected.messages()), vec![&small]);
        let two = project_tool_images(
            conversation([
                small.clone().with_text_offset(1),
                small.clone().with_text_offset(2),
            ]),
            &caps,
        );
        assert_eq!(images_in(two.messages())[0].text_offset(), Some(2));
    }
    #[test]
    fn tool_dimensions_and_pixels_share_strict_native_policy_without_reordering_gaps() {
        // Catches dimensions/pixels being forgotten by optional tool projection.
        let mut bytes = JPEG_1X1.to_vec();
        let sof = bytes
            .windows(9)
            .position(|w| w == [255, 192, 0, 17, 8, 0, 1, 0, 1])
            .unwrap();
        bytes[sof + 6] = 2;
        bytes[sof + 8] = 3;
        let wide = Image::new(bytes, ImageMediaType::Jpeg).unwrap();
        assert_eq!((wide.width(), wide.height()), (3, 2));
        let limits = ImageLimits::new(
            BTreeSet::from([ImageMediaType::Png, ImageMediaType::Jpeg]),
            None,
            None,
            None,
            NonZeroU32::new(1),
            NonZeroU32::new(1),
            NonZeroU64::new(1),
        )
        .unwrap();
        let caps = caps(ImageInputSupport::Supported(limits));
        let native = request(conversation([wide.clone()]));
        assert_eq!(
            caps.request_gaps(&native),
            vec![
                CapabilityGap::ImageTooWide {
                    index: 1,
                    width: 3,
                    max: 1
                },
                CapabilityGap::ImageTooHigh {
                    index: 1,
                    height: 2,
                    max: 1
                },
                CapabilityGap::ImageTooManyPixels {
                    index: 1,
                    pixels: 6,
                    max: 1
                }
            ]
        );
        let small = image().with_tool_result_origin();
        let projected = project_tool_images(
            conversation([small.clone(), wide.with_tool_result_origin()]),
            &caps,
        );
        assert_eq!(images_in(projected.messages()), vec![&small]);
    }
    #[test]
    fn unsupported_format_leaves_native_attachment_strict() {
        // Catches MIME/dimension policy divergence and treating native input as
        // optional evidence. Strict native gap values and order stay unchanged.
        let mut dimension = ImageLimits::new(
            BTreeSet::from([ImageMediaType::Png]),
            None,
            None,
            None,
            NonZeroU32::new(1),
            NonZeroU32::new(1),
            NonZeroU64::new(1),
        )
        .unwrap();
        let native = image();
        let tool = image().with_tool_result_origin();
        dimension.accepted_media_types = BTreeSet::from([ImageMediaType::Jpeg]);
        let projected = project_tool_images(
            conversation([native.clone(), tool]),
            &caps(ImageInputSupport::Supported(dimension)),
        );
        assert_eq!(images_in(projected.messages()), vec![&native]);
        let unsupported = caps(ImageInputSupport::Unsupported);
        let projected = project_tool_images(
            conversation([native.clone(), image().with_tool_result_origin()]),
            &unsupported,
        );
        assert_eq!(images_in(projected.messages()), vec![&native]);
        assert!(matches!(
            unsupported.request_gaps(&request(projected)).as_slice(),
            [CapabilityGap::ImageInputUnsupported {
                supplied: 1,
                carried: 0
            }]
        ));
    }
}
