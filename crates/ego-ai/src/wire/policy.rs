//! The shared response rules that do not depend on the wire format.
//!
//! A wire adapter translates provider bytes into normalized events and states
//! what its wire said; it does not decide what a malformed answer means, so no
//! provider can decide differently. Rules that need the request,
//! such as declared tool names and attributable call ids, are in the
//! generator's [`ToolNames`](crate::tool_names::ToolNames) stage. Calls of a
//! failed response need no rule here: the turn runs calls only after
//! `Stop(ToolUse)`, and holding them back would delay the loop guard that
//! watches calls as they arrive.

use ego_core::generator::port::AiEvent;
use ego_core::generator::types::ErrorKind;

/// The one notice for a usage report whose cache parts exceed its input total.
///
/// The usage itself is discarded rather than clamped: a clamped figure reads
/// exactly like a correct one while it under-reports the dearest category.
/// With no usage the attempt settles at its pessimistic bound.
///
/// `Unclassified` on purpose. The generation did not fail, so `Transient`
/// would invite a retry of a turn that succeeded, and neither the request nor
/// the account is what went wrong.
pub(crate) fn incoherent_usage() -> AiEvent {
    AiEvent::Error {
        message: "the provider's prompt cache breakdown was larger than the prompt \
                  it broke down, so no usage figure could be trusted; this \
                  generation settles at its bound"
            .to_string(),
        kind: ErrorKind::Unclassified,
    }
}
