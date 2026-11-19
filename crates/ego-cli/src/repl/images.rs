//! Draft-owned images and the terminal's image-admission rendezvous.

use std::sync::{Arc, Mutex};

use ego_core::generator::capabilities::ImageLimits;
use ego_core::generator::types::{
    Image, ImageRef, MAX_IMAGE_BYTES, MAX_IMAGES_PER_PROMPT, MAX_TOTAL_IMAGE_BYTES,
};

#[derive(Debug, Clone)]
pub(crate) enum DraftImageSource {
    Path { path: String, bytes: u64 },
    Inline(Image),
    Published(ImageRef),
}

#[derive(Debug, Clone)]
pub(crate) struct DraftImage {
    pub label: String,
    pub source: DraftImageSource,
}

impl DraftImage {
    fn bytes(&self) -> u64 {
        match &self.source {
            DraftImageSource::Path { bytes, .. } => *bytes,
            DraftImageSource::Inline(image) => image.bytes().len() as u64,
            DraftImageSource::Published(image) => image.reference().bytes,
        }
    }
}

/// A handle moves with its text draft, never with the driver's current turn.
#[derive(Debug, Clone, Default)]
pub struct ImageDraft(Arc<Mutex<Vec<DraftImage>>>);

impl ImageDraft {
    pub(crate) fn snapshot(&self) -> Vec<DraftImage> {
        self.0.lock().expect("draft images lock").clone()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.lock().expect("draft images lock").is_empty()
    }

    pub(crate) fn labels(&self) -> Vec<String> {
        self.0
            .lock()
            .expect("draft images lock")
            .iter()
            .map(|image| image.label.clone())
            .collect()
    }

    pub(crate) fn clear(&self) {
        self.0.lock().expect("draft images lock").clear();
    }

    pub(crate) fn pop(&self) {
        self.0.lock().expect("draft images lock").pop();
    }

    pub(crate) fn push(&self, image: DraftImage, limits: &ImageLimits) -> Result<(), String> {
        let mut images = self.0.lock().expect("draft images lock");
        let count = limits
            .max_images()
            .map_or(MAX_IMAGES_PER_PROMPT, |n| n.get() as usize)
            .min(MAX_IMAGES_PER_PROMPT);
        let per_image = limits
            .max_bytes_per_image()
            .map_or(MAX_IMAGE_BYTES, |n| n.get())
            .min(MAX_IMAGE_BYTES);
        let total = limits
            .max_total_bytes()
            .map_or(MAX_TOTAL_IMAGE_BYTES, |n| n.get())
            .min(MAX_TOTAL_IMAGE_BYTES);
        if images.len() >= count {
            return Err(format!("image draft accepts at most {count} images"));
        }
        if image.bytes() > per_image {
            return Err(format!(
                "image paste is {} bytes, above the {per_image}-byte limit",
                image.bytes()
            ));
        }
        let bytes = images.iter().fold(image.bytes(), |total, image| {
            total.saturating_add(image.bytes())
        });
        if bytes > total {
            return Err(format!(
                "image draft exceeds the {total}-byte aggregate limit"
            ));
        }
        images.push(image);
        Ok(())
    }

    pub(crate) fn published(&self, references: &[ImageRef]) {
        let mut images = self.0.lock().expect("draft images lock");
        let labels = images
            .iter()
            .map(|image| image.label.clone())
            .collect::<Vec<_>>();
        *images = references
            .iter()
            .enumerate()
            .map(|(index, reference)| DraftImage {
                label: labels
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| format!("attachment {}", index + 1)),
                source: DraftImageSource::Published(reference.clone()),
            })
            .collect();
    }

    pub(crate) fn recalled(images: &[ImageRef]) -> Self {
        Self(Arc::new(Mutex::new(
            images
                .iter()
                .enumerate()
                .map(|(index, image)| DraftImage {
                    label: format!("recalled {}", index + 1),
                    source: DraftImageSource::Published(image.clone()),
                })
                .collect(),
        )))
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ImageOperation {
    Clipboard,
    Path(String),
    Admit,
}

#[derive(Debug)]
struct Request {
    draft: ImageDraft,
    operation: ImageOperation,
    reply: crossbeam::channel::Sender<Result<Vec<ImageRef>, String>>,
}

/// An owned request, shared only to retain the editor command's clone semantics.
#[derive(Debug, Clone)]
pub struct ImageRequest(Arc<Request>);

impl PartialEq for ImageRequest {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ImageRequest {}

impl ImageRequest {
    pub(crate) fn new(
        draft: ImageDraft,
        operation: ImageOperation,
    ) -> (
        Self,
        crossbeam::channel::Receiver<Result<Vec<ImageRef>, String>>,
    ) {
        let (reply, answer) = crossbeam::channel::bounded(1);
        (
            Self(Arc::new(Request {
                draft,
                operation,
                reply,
            })),
            answer,
        )
    }
    pub(crate) fn draft(&self) -> &ImageDraft {
        &self.0.draft
    }
    pub(crate) fn operation(&self) -> &ImageOperation {
        &self.0.operation
    }
    pub(crate) fn reply(&self, result: Result<Vec<ImageRef>, String>) {
        let _ = self.0.reply.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::generator::types::ImageMediaType;

    fn limits(total: Option<std::num::NonZeroU64>) -> ImageLimits {
        ImageLimits::new(
            [ImageMediaType::Png].into_iter().collect(),
            None,
            None,
            total,
            None,
            None,
            None,
        )
        .unwrap()
    }
    fn path(label: &str, bytes: u64) -> DraftImage {
        DraftImage {
            label: label.to_owned(),
            source: DraftImageSource::Path {
                path: label.to_owned(),
                bytes,
            },
        }
    }

    // Catches: repeated path/clipboard paste bypasses the product image count.
    #[test]
    fn draft_count_limit_retains_the_existing_batch() {
        let draft = ImageDraft::default();
        for index in 0..MAX_IMAGES_PER_PROMPT {
            draft
                .push(path(&index.to_string(), 1), &limits(None))
                .unwrap();
        }
        let before = draft.labels();
        assert!(draft.push(path("overflow", 1), &limits(None)).is_err());
        assert_eq!(draft.labels(), before);
    }

    // Catches: separate paste operations each reset the aggregate byte budget.
    #[test]
    fn draft_aggregate_limit_retains_the_existing_batch() {
        let draft = ImageDraft::default();
        let limits = limits(std::num::NonZeroU64::new(3));
        draft.push(path("first", 2), &limits).unwrap();
        assert!(draft.push(path("second", 2), &limits).is_err());
        assert_eq!(draft.labels(), ["first"]);
        draft.push(path("boundary", 1), &limits).unwrap();
        assert_eq!(draft.labels(), ["first", "boundary"]);
    }
}
