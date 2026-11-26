use crate::compact::ContextPercent;
use crate::state::{ContextWindow, SessionState};

/// Conservative bytes per token for text nothing has measured yet. Real text
/// averages three to four bytes a token; CJK and whitespace floods are denser,
/// and an over-estimate only compacts early.
const BYTES_PER_TOKEN: u64 = 3;

/// Unsent deltas smaller than this are within the estimator's error.
const NOISE_FLOOR_TOKENS: u64 = 1_024;

/// The measured occupancy plus an estimate of what the next request adds.
///
/// `used_tokens` stays `None` while nothing has been measured: an unmeasured
/// session has no base to add a delta to, and inventing one would refuse or
/// compact a fresh session.
pub(super) fn projected(context: ContextWindow, unsent_tokens: u64) -> ContextWindow {
    ContextWindow {
        used_tokens: context
            .used_tokens
            .map(|used| used.saturating_add(unsent_tokens)),
        ..context
    }
}

/// Estimated tokens of history the last measured request did not contain.
///
/// Text and arguments of items created after the last measurement, plus the
/// results of calls issued since the measurement before it (their results
/// arrive after the generation that measured). Arguments are counted as they
/// will be replayed, bounded.
pub(super) fn unsent_delta_tokens(state: &SessionState) -> u64 {
    let mut bytes = 0_u64;
    for item in state.items.values() {
        if item.seq > state.context_measured_seq {
            bytes += item.text.len() as u64;
            bytes += crate::generator::history::bounded_replay_args(&item.args).len() as u64;
        }
        if item.seq > state.context_prev_measured_seq
            && let Some(result) = &item.tool_result
        {
            bytes += result.model_text().len() as u64;
        }
    }
    let tokens = bytes / BYTES_PER_TOKEN;
    // Below the estimate's own error a projection would only turn a request the
    // measurement says fits into a refusal, so the measurement stands.
    if tokens < NOISE_FLOOR_TOKENS {
        0
    } else {
        tokens
    }
}

/// Refuse known exhaustion without inventing a token count for unsent history.
///
/// `unsent_tokens` is how much of `context.used_tokens` is a projection, so
/// the message says what was measured and what was estimated. It never names
/// a command that cannot run: `/compact` prepares its summarizer input to fit
/// even an over-window history, and the other offered paths are always open.
pub(super) fn exhausted(
    context: ContextWindow,
    window: Option<u64>,
    unsent_tokens: u64,
) -> Option<String> {
    let (used, window) = (context.used_tokens?, window?);
    (used >= window).then(|| {
        let basis = if unsent_tokens == 0 {
            format!("The last measured context occupancy is {used} tokens")
        } else {
            format!(
                "The projected context is {used} tokens (the last measurement plus about \
                 {unsent_tokens} unsent)"
            )
        };
        format!(
            "{basis} against a {window}-token model window, so the request was not sent. \
             Run /compact (it fits an over-window history into the summarizer), select a \
             model with a larger window, or start a new session and carry the task over \
             in your first prompt."
        )
    })
}

/// A threshold on occupancy as measured or projected by the caller.
///
/// Pure arithmetic on whatever `context` holds: pass the measured value for a
/// post-hoc check, or [`projected`] to decide before sending. `false` while
/// occupancy or the window is unknown.
pub fn compaction_due(context: ContextWindow, threshold: ContextPercent) -> bool {
    match (context.used_tokens, context.window) {
        (Some(used), Some(window)) if window > 0 => {
            u128::from(used) * 100 >= u128::from(window) * u128::from(threshold.get())
        }
        _ => false,
    }
}
