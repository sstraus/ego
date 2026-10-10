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
