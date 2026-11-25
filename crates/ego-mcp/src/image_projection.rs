//! Bounded MCP image acquisition; only validated session references cross into core.

use base64::Engine as _;
use ego_core::generator::types::{
    Image, ImageMediaType, ImageRef, MAX_IMAGE_BYTES, MAX_IMAGES_PER_PROMPT, MAX_TOTAL_IMAGE_BYTES,
};
use ego_core::mcp::ForeignContent;
use ego_core::permit::ToolError;
use ego_core::store::evidence::EvidenceRef;
use serde_json::Value;

type Publish<'a> = dyn Fn(&[u8]) -> Result<EvidenceRef, ToolError> + 'a;

pub(super) struct ImageProjection<'a> {
    publish: &'a Publish<'a>,
    count: usize,
    encoded_bytes: u64,
    decoded_bytes: u64,
}

impl<'a> ImageProjection<'a> {
    pub(super) fn new(publish: &'a Publish<'a>) -> Self {
        Self {
            publish,
            count: 0,
            encoded_bytes: 0,
            decoded_bytes: 0,
        }
    }

    pub(super) fn project(&mut self, value: &Value) -> ForeignContent {
        match self.validate_and_publish(value) {
            Ok(image) => ForeignContent::Image { image },
            // A completed side-effecting call must not become retryable because
            // one image is malformed. Never echo server data or an IO error.
            Err(cause) => ForeignContent::text(format!("[MCP image unavailable: {cause}]")),
        }
    }

    fn validate_and_publish(&mut self, value: &Value) -> Result<ImageRef, &'static str> {
        self.count = self.count.saturating_add(1);
        if self.count > MAX_IMAGES_PER_PROMPT {
            return Err("image count limit exceeded");
        }
        let media_type = match value.get("mimeType").and_then(Value::as_str) {
            Some("image/png") => ImageMediaType::Png,
            Some("image/jpeg") => ImageMediaType::Jpeg,
            Some("image/gif") => ImageMediaType::Gif,
            Some("image/webp") => ImageMediaType::Webp,
            _ => return Err("unsupported media type"),
        };
        let data = value
            .get("data")
            .and_then(Value::as_str)
            .ok_or("missing base64 data")?;
        // Bound before allocating decoded bytes, including failed images. The
        // aggregate encoded allowance accounts for each image's padding.
        let encoded_limit = MAX_IMAGE_BYTES.div_ceil(3) * 4;
        let aggregate_encoded_limit =
            MAX_TOTAL_IMAGE_BYTES.div_ceil(3) * 4 + (MAX_IMAGES_PER_PROMPT as u64) * 4;
        let encoded = u64::try_from(data.len()).map_err(|_| "encoded byte limit exceeded")?;
        self.encoded_bytes = self.encoded_bytes.saturating_add(encoded);
        if encoded > encoded_limit || self.encoded_bytes > aggregate_encoded_limit {
            return Err("encoded byte limit exceeded");
        }
        // STANDARD rejects noncanonical padding/trailing bits, not just alphabet errors.
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| "invalid base64")?;
        let decoded = bytes.len() as u64;
        self.decoded_bytes = self.decoded_bytes.saturating_add(decoded);
        if decoded > MAX_IMAGE_BYTES || self.decoded_bytes > MAX_TOTAL_IMAGE_BYTES {
            return Err("decoded byte limit exceeded");
        }
        let image = Image::new(bytes, media_type)
            .map_err(|_| "invalid image encoding, media type, dimensions or pixels")?;
        let reference = (self.publish)(image.bytes())
            .map_err(|_| "session image storage unavailable")?
            .blob_ref();
        ImageRef::new(reference, media_type, image.width(), image.height())
            .map_err(|_| "invalid stored image reference")
    }
}
