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
  row. The painter records the rows, the cursor position and the terminal size
  at the return (`PromptFootprint`), and erases nothing when the size or the
  cursor no longer match. regression case uses it to replace an answered question
  with one transcript row; the API carries no εgo policy and is written to be
  proposed upstream.

Do not add εgo policy, DTOs, input handling, or rendering to this crate. The
decision and replacement condition are recorded in
`ARCHITECTURE.md`.
