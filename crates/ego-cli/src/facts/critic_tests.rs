//! Regressions in the generated builtin rows against facts the hand-written
//! catalogue had read from an authority.

use super::*;
use ego_core::generator::capabilities::ReasoningEffortSupport;
use ego_core::generator::types::ThinkingLevel;

/// Catches: a codex row generated from the API (`openai`) snapshot row, which
/// lists `none`, so `thinking = off` passes validation for a subscription model
/// whose authenticated Codex cache (read 2026-09-26, hand row at 99716b79) lists
/// low..max only and the request goes out with an effort the backend never offered.
#[test]
fn the_codex_gpt_6_sol_row_does_not_gain_off_from_the_api_row() {
    let facts = builtin();
    let row = facts
        .model
        .get("openai-codex/gpt-6-sol")
        .expect("the codex gpt-6-sol row");
    let ReasoningEffortSupport::Only(levels) = &row.capabilities.reasoning_efforts else {
        panic!("the codex row states its efforts");
    };
    assert!(
        !levels.contains(&ThinkingLevel::Off),
        "codex gpt-6-sol offers off: {levels:?}"
    );
}
