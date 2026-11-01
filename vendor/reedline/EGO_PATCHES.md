# εgo's Reedline source pin

This directory is the published `reedline` 0.51.0 crate from tag `v0.51.0`,
VCS revision `1a9a8d5512ba59b75a5f55e6322f3c2a2abb640e`. The upstream MIT license is
preserved in `LICENSE`.

The source is carried locally because regression case depends on the current public
host-break, repaint, and bounded external-printer APIs, while the published
external-printer painter still reconstructs its pre-print cursor movement from
UTF-8 byte counts. That corrupts narrow layouts containing combining or wide
graphemes and can divide by a transient zero terminal width.

The permitted local delta is intentionally narrow:

- commit host submission receipts through the painter without editing-viewport
  clipping, preserving the original buffer and history (regression case). The receipt
  replaces the live prompt footprint and can scroll in full. Its final anchor
  follows the emitted grapheme wraps and line feeds without a cursor query;
  host formatting remains outside this crate;
- add a default-empty `Prompt::render_prompt_footer` hook, counted with the
  prompt footprint and painted below the buffer (regression case, approved 2026-10-18);
- position external output from the painter's tracked prompt anchor instead of
  estimating rows from buffer bytes or display-width division;
- erase the complete prompt footprint from that anchor before external output
  can wrap over its rows (regression case). Clearing only the first row leaves stale
  prompt suffixes after a wrapped message, which repaint can scroll above the
  new prompt anchor;
- add regression coverage for narrow, combining, wide, and zero-width cases;
- reuse a suspended prompt's rows on resume unless the cursor is on the bottom
  row (regression case). Upstream nushell/reedline#1130 makes a new prompt whenever
  the prompt range touches the row above the bottom; that also catches a flush
  prompt with an empty row below it, and the resume then leaves the old frame
  in view. Only a cursor on the bottom row is ambiguous after a scroll, so only
  that case makes a new prompt;
- add `Reedline::erase_last_prompt`, which erases the prompt and buffer that
  the last `read_line` left on screen so the next prompt starts on its first
  row. The painter records the rows and terminal size at the return
  (`PromptFootprint`), and erases nothing when the size no longer matches.
  The host must call it before any other writer moves the screen. regression case
  uses it to replace an answered question with one transcript row;
- retire a host-accepted buffer without querying the terminal for coordinates
  that the same owner just painted (regression case). `accept_buffer` advances the
  cached anchor through its final CRLF, including bottom-row scrolling, and
  retains that anchor for the next read. Successful `erase_last_prompt` likewise
  retains its emitted start row and resets the empty prompt geometry; an image
  or modal return must not rediscover coordinates still owned by this editor.
  `invalidate_prompt_position` explicitly retires that cache when another process
  owned the tty (for example, shell job suspension). Missing cursor-position replies must
  not kill the input owner during a queued submission. These APIs carry no
  εgo policy and are written to be proposed upstream.

Do not add εgo policy, DTOs, input handling, or rendering to this crate. Remove the local patches when equivalent upstream APIs and corrections are available.

For the public terminal controls that use this integration, see
`docs/guides/interactive.md`.
