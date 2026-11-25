# PTY EOF regression capture

`typed-fork-before-eof.ansi` is the raw transcript of Linux/aarch64 rb child
3903522 from the failing `fork_metadata::interactive_fork_carries_the_source_title_and_goal`
run on pre-fix commit `2381904d73cdf5989c58890475130f134b35c277`.

Source: `~/Gits/.tmp/ego-259/reproduction-proc.log`, rb artifact
`20261004T170430Z-25617`. The temporary diagnostic printed the transcript
immediately after the harness sent Ctrl-D. The fixture decodes that diagnostic's
Rust string escapes back to the original PTY bytes; no terminal bytes were invented.

The last complete frame still contains `/fork`. The old harness counts its `> `
as a new ready prompt. At the 60-second timeout, `/proc` showed the `ego-reedline`
thread waiting in `ep_poll` for terminal input. This capture protects against
sending EOF on a typed-line repaint or an incomplete redraw.

# Goal receipt split across printer handoffs

`goal-receipt-split-rows.ansi` is the raw PTY output, from the prompt submit
onwards, of `goal_visibility::goal_visibility_needs_operator_blocks_without_spending_allowance`
on macOS at commit `f9628b3cc756f593ae7ab27099e7b603f63aadd1` (story 1095-8893).
A temporary, uncommitted 150 ms sleep after each row sent by
`SurfaceOutput::write_with_kind` made every physical row its own printer
handoff. That is the order that load produced on rb in the full suite.
No terminal bytes were invented.

Between the two rows of the wrapped goal receipt, the composer repaints its
`◐ Working` row. The next handoff moves to the composer anchor, erases it with
`ESC [ J` and prints the second row there. The terminal screen shows one whole
receipt; a reader that joins the raw bytes sees the Working frame in the middle
of the sentence.
