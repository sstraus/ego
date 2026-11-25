//! Submitted-image receipts shared by live output and durable replay.

/// Receipt-only marker; image bytes and draft paths never enter the transcript.
pub(crate) fn label(count: usize) -> Option<String> {
    (count > 0).then(|| {
        format!(
            "… [{count} image{} attached]",
            if count == 1 { "" } else { "s" }
        )
    })
}
