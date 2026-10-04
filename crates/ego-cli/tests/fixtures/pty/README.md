# PTY EOF regression capture

`typed-fork-before-eof.ansi` is the raw transcript of Linux/aarch64 rb child
3903522 from the failing `fork_metadata::interactive_fork_carries_the_source_title_and_goal`
run on pre-fix commit `2381904d73cdf5989c58890475130f134b35c277`.

Source: `~/Gits/.tmp/example-evidence-a/reproduction-proc.log`, rb artifact
`20261004T170430Z-25617`. The temporary diagnostic printed the transcript
immediately after the harness sent Ctrl-D. The fixture decodes that diagnostic's
Rust string escapes back to the original PTY bytes; no terminal bytes were invented.

The last complete frame still contains `/fork`. The old harness counts its `> `
as a new ready prompt. At the 60-second timeout, `/proc` showed the `ego-reedline`
thread waiting in `ep_poll` for terminal input. This capture protects against
sending EOF on a typed-line repaint or an incomplete redraw.
