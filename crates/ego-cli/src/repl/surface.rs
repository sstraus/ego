//! Single-writer integration state for transcript rows and ephemeral status.

use std::sync::Mutex;

use reedline::RepaintSignal;

use super::status::StatusLine;

#[derive(Debug, Default)]
struct PaintedSurface {
    transcript: String,
    status: Option<StatusLine>,
    capture_transcript: bool,
}

/// The sole logical writer for an interactive transcript and status row.
///
/// The terminal adapter may feed these operations into Reedline's existing
/// painter. This type deliberately requests neither alternate-screen nor
/// mouse mode, so an ephemeral status never changes transcript semantics.
#[derive(Debug)]
pub struct InteractivePainter {
    painted: Mutex<PaintedSurface>,
    repaint: Option<RepaintSignal>,
}

impl InteractivePainter {
    /// Build an ordinary inline painter.
    pub fn new() -> Self {
        Self {
            painted: Mutex::new(PaintedSurface {
                capture_transcript: true,
                ..PaintedSurface::default()
            }),
            repaint: None,
        }
    }

    /// Bind status replacement to Reedline's existing terminal owner.
    pub(crate) fn live(repaint: RepaintSignal) -> Self {
        Self {
            painted: Mutex::new(PaintedSurface::default()),
            repaint: Some(repaint),
        }
    }

    /// Append a durable transcript batch through the same writer.
    pub fn paint_transcript(&self, text: &str) {
        let mut painted = self.painted.lock().expect("interactive painter lock");
        if painted.capture_transcript {
            painted.transcript.push_str(text);
        }
    }

    /// Replace the ephemeral status row without adding it to the transcript.
    pub fn paint_status(&self, status: StatusLine) {
        self.painted
            .lock()
            .expect("interactive painter lock")
            .status = Some(status);
        if let Some(repaint) = &self.repaint {
            repaint.request_repaint();
        }
    }

    /// Read the complete latest row for Reedline's prompt projection.
    pub(crate) fn current_status(&self) -> Option<StatusLine> {
        self.painted
            .lock()
            .expect("interactive painter lock")
            .status
            .clone()
    }

    /// Remove the native row while retaining the same Reedline owner.
    pub(crate) fn clear_status(&self) {
        self.painted
            .lock()
            .expect("interactive painter lock")
            .status = None;
        if let Some(repaint) = &self.repaint {
            repaint.request_repaint();
        }
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

impl Default for InteractivePainter {
    fn default() -> Self {
        Self::new()
    }
}
