# Git worktrees

Read this page in the [hosted user guide](https://sstraus.github.io/ego/guides/worktrees.html).

Use a worktree when a task needs an isolated checkout and a new branch. A worktree
shares Git history with its source repository, but has its own working files.
Creating a worktree does not grant tools more filesystem or network authority.

## Start isolated work

From a Git checkout, name the new branch explicitly:

```sh
ego --worktree feat/parser-check "Review the parser and propose a focused fix"
```

For headless work, use `ego run --worktree feat/parser-check PROMPT` with the
model and authority needed by that run. The branch must be new. This option
starts a new session; it cannot be combined with a session selection or used to
move an existing session. Review the printed receipt and checkout path.

The checkout remains after the process exits. There is no automatic cleanup
that can discard unfinished work.

## Inspect and clean up

Use the receipt UUID, not a guessed filesystem path:

```sh
ego worktree show RECEIPT_UUID
ego worktree cleanup RECEIPT_UUID
```

Cleanup refuses an active launch, changed repository identity or branch, local
changes, untracked or ignored files, and unmerged commits. New commits also need
upstream evidence before cleanup. The branch is retained after checkout removal.
Inspect a refusal rather than deleting the directory to bypass the checks.

When `TUIC_SESSION` is nonempty and `tuic` is executable on PATH,
TUICommander owns launch and worktree lifecycle. Otherwise ego uses system git. Use its
controls; a hosted receipt is not adopted into ego's local worktree store.

Sources: [worktree lifecycle](../../crates/ego-cli/src/worktree.rs) and
[worktree tests](../../crates/ego-cli/tests/worktree.rs).
