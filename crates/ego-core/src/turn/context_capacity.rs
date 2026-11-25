use crate::compact::ContextPercent;
use crate::state::ContextWindow;

/// Refuse known exhaustion without inventing a token count for unsent history.
pub(super) fn exhausted(context: ContextWindow, window: Option<u64>) -> Option<String> {
    let (used, window) = (context.used_tokens?, window?);
    (used >= window).then(|| {
        format!(
            "The last measured context occupancy is {used} tokens against a {window}-token \
             model window. Run /compact before continuing, or select a model with a larger window."
        )
    })
}

/// A measured threshold, not an estimate of the unsent request's size.
pub fn compaction_due(context: ContextWindow, threshold: ContextPercent) -> bool {
    match (context.used_tokens, context.window) {
        (Some(used), Some(window)) if window > 0 => {
            u128::from(used) * 100 >= u128::from(window) * u128::from(threshold.get())
        }
        _ => false,
    }
}
