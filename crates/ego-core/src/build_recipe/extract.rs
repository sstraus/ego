//! Apply one recipe to complete, successful process output.
//!
//! The only operation is omitting whole lines that match a literal rule.
//! Every other line stays, so output the recipe did not anticipate reaches
//! the model. A line that names an error, warning, failure or panic is never
//! omitted. Any doubt is a [`Fallback`]: the caller then keeps the original
//! evidence-backed answer.

use super::StoredRecipe;
use crate::permit::StreamProvenance;
use crate::store::evidence::EvidenceRef;

/// Most complete output bytes one projection reads.
pub const MAX_INPUT_BYTES: u64 = 4 * 1024 * 1024;
/// Most bytes of the kept lines in one answer. The header comes on top and
/// stays inside the 8 KiB projection limit of `ToolResult`.
pub const MAX_KEPT_BYTES: usize = 6 * 1024;

/// Words that keep a line whatever the rules say (compared in lowercase).
/// `warn` covers `warning`; `deprecat` covers `deprecated` and `deprecation`.
const PROTECTED: [&str; 7] = [
    "error",
    "warn",
    "fail",
    "fatal",
    "panic",
    "exception",
    "deprecat",
];

/// One complete stream of one process.
pub struct StreamInput<'a> {
    pub stream: StreamProvenance,
    pub bytes: &'a [u8],
    pub reference: &'a EvidenceRef,
}

/// Why a recipe was not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fallback {
    /// The output is larger than [`MAX_INPUT_BYTES`].
    InputTooLarge,
    /// A stream is not UTF-8 text.
    NotText,
    /// A format marker is missing, so the output is not the learned format.
    UnknownFormat { marker: String },
    /// The rules omit nothing, or the answer is not smaller than the output.
    NoReduction,
    /// The kept lines are still larger than [`MAX_KEPT_BYTES`].
    AnswerTooLarge,
}

impl std::fmt::Display for Fallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InputTooLarge => write!(f, "output exceeds {MAX_INPUT_BYTES} bytes"),
            Self::NotText => f.write_str("output is not UTF-8 text"),
            Self::UnknownFormat { marker } => {
                write!(f, "output does not contain the format marker {marker:?}")
            }
            Self::NoReduction => f.write_str("the answer would not be smaller than the output"),
            Self::AnswerTooLarge => write!(f, "kept lines exceed {MAX_KEPT_BYTES} bytes"),
        }
    }
}

/// The compact model answer for `streams`, or why there is none.
pub fn project(stored: &StoredRecipe, streams: &[StreamInput<'_>]) -> Result<String, Fallback> {
    let total: u64 = streams.iter().map(|input| input.bytes.len() as u64).sum();
    if total > MAX_INPUT_BYTES {
        return Err(Fallback::InputTooLarge);
    }
    let texts = streams
        .iter()
        .map(|input| std::str::from_utf8(input.bytes).map_err(|_| Fallback::NotText))
        .collect::<Result<Vec<_>, _>>()?;
    let recipe = &stored.recipe;
    if let Some(marker) = recipe
        .require
        .iter()
        .find(|marker| !texts.iter().any(|text| text.contains(marker.as_str())))
    {
        return Err(Fallback::UnknownFormat {
            marker: marker.clone(),
        });
    }

    let mut lines_total = 0usize;
    let mut omitted = 0usize;
    let mut kept_bytes = 0usize;
    let mut sections = Vec::new();
    for (input, text) in streams.iter().zip(&texts) {
        let mut kept = Vec::new();
        for line in text.lines() {
            lines_total += 1;
            if omits(stored, line) {
                omitted += 1;
            } else {
                kept_bytes += line.len() + 1;
                if kept_bytes > MAX_KEPT_BYTES {
                    return Err(Fallback::AnswerTooLarge);
                }
                kept.push(line);
            }
        }
        sections.push(format!(
            "[{}; complete output: read_tool_result {}]\n{}",
            input.stream,
            input.reference,
            kept.join("\n")
        ));
    }
    if omitted == 0 {
        return Err(Fallback::NoReduction);
    }
    let answer = format!(
        "[exec succeeded (exit 0). Build recipe {} revision {} omitted {omitted} of {lines_total} lines; every other line is shown. Read the complete output with read_tool_result.]\n\n{}",
        stored.id,
        stored.revision,
        sections.join("\n\n")
    );
    if answer.len() as u64 >= total {
        return Err(Fallback::NoReduction);
    }
    Ok(answer)
}

fn omits(stored: &StoredRecipe, line: &str) -> bool {
    let line = line.trim_end_matches('\r');
    if !stored.recipe.omit.iter().any(|rule| rule.matches(line)) {
        return false;
    }
    let lower = line.to_ascii_lowercase();
    !PROTECTED.iter().any(|word| lower.contains(word))
}
