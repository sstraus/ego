//! Single-writer integration state for transcript rows and ephemeral status.

use std::sync::Mutex;

use super::status::StatusLine;

#[derive(Debug, Default)]
struct PaintedSurface {
    transcript: String,
    status: Option<StatusLine>,
}

/// The sole logical writer for an interactive transcript and status row.
///
/// The terminal adapter may feed these operations into Reedline's existing
/// painter. This type deliberately requests neither alternate-screen nor
/// mouse mode, so an ephemeral status never changes transcript semantics.
#[derive(Debug, Default)]
pub struct InteractivePainter {
    painted: Mutex<PaintedSurface>,
}

impl InteractivePainter {
    /// Build an ordinary inline painter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a durable transcript batch through the same writer.
    pub fn paint_transcript(&self, text: &str) {
        self.painted
            .lock()
            .expect("interactive painter lock")
            .transcript
            .push_str(text);
    }

    /// Replace the ephemeral status row without adding it to the transcript.
    pub fn paint_status(&self, status: StatusLine) {
        self.painted
            .lock()
            .expect("interactive painter lock")
            .status = Some(status);
    }

    /// There is exactly one serialized writer for both kinds of paint.
    pub fn writer_count(&self) -> usize {
        1
    }

    /// Snapshot the durable transcript for an adapter or diagnostic.
    pub fn transcript(&self) -> String {
        self.painted
            .lock()
            .expect("interactive painter lock")
            .transcript
            .clone()
    }

    /// Whether the current status is kept outside the transcript.
    pub fn status_is_ephemeral(&self) -> bool {
        self.painted
            .lock()
            .expect("interactive painter lock")
            .status
            .is_some()
    }

    /// Inline painting does not enter an alternate screen.
    pub fn uses_alternate_screen(&self) -> bool {
        false
    }

    /// Inline painting does not request mouse capture.
    pub fn captures_mouse(&self) -> bool {
        false
    }
}
