# Story 397: native residual-write validation

Validated implementation: `38a199ba8631e36ddf87a1e53d190898e1d9bdea`.
Date: 2026-10-13. Native platform: macOS. No paid model calls.

## RED

The critic ran `adaptive_judge_cannot_overwrite_agent_instruction_files` at
`7792744c4e91f405f67b49859560ebfaed4ba655`, with production code from `bd7c15d3`.
It failed because a forced judge verdict overwrote four temporary instruction
files: `.codex/ORCHESTRATION.md`, `.claude/CLAUDE.md`, `Gits/other/AGENTS.md`, and
`Gits/other/CLAUDE.md`. The regression body remains unchanged.

## Native GREEN

Run from the worktree root through the local build scheduler and mbx:

```sh
PATH="$HOME/Library/Application Support/mbx/bin:/opt/homebrew/bin:$PATH" \
TMPDIR="$HOME/Gits/.tmp/fix-397-tests/" \
~/Gits/personal/orchestrator/tools/build/build-slot.sh \
  cargo nextest run -p ego-tools -p ego-cli \
  -E 'binary(adaptive_admission) | binary(residual_session)' --no-fail-fast
```

Result: **27 passed, 0 skipped** in the selected binaries: 25 Adaptive admission
tests and 2 CLI/ACP session tests. Compilation took 49.76 seconds; tests took
10.989 seconds. Nextest run: `c9093c32-7ecd-4483-b5bf-89f8ad8f1545`.

This includes:

- The unchanged critic regression: instruction file bytes remain intact.
- A broader judge directory grant: ordinary output is writable while nested
  `AGENTS.md` and `CLAUDE.md` remain read-only.
- CLI: a real residual refusal reaches the shared judge and its session grant
  reaches the next explicit exec without automatic replay.
- ACP: the same authority survives into the session's next prompt/run.

The CLI/ACP tests use the existing loopback provider harness. They assert one
journaled residual decision tied to the judge generation and an output file
written exactly once after explicit retry.

mbx reported action-prediction capacity warnings. There were no compiler or test
failures. Local log: `~/Gits/.tmp/results/fix-397-native.log`.

## Other evidence and limits

The targeted rb Linux run passed 14 tests, including the executor instruction
floor recheck, existing residual authority tests and the bounded denial-preview
test. Its 3515 skipped tests are not a full-suite pass.

Native evidence proves critic findings D1 and D2. D3 remains accepted: the
structural attack test is retained, with `control-file-write` removed from its
prompt-only exceptions because it now has an executor floor. Injection risks
remain prompt-only; plan mode retains its execution gate.

Full post-fix package suites, Clippy and paid judge evaluation were not run.
Story criteria 2, 3 and 4 are checked; the aggregate GREEN and manual paid-eval
criteria remain open. The story is not complete.
