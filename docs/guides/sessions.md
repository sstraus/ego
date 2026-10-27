# Sessions and history

Durable sessions let you continue work after a terminal exits, inspect what
happened, or branch without overwriting the source. Terminal state and durable
conversation state are different: clearing the composer does not erase history.

## Start, find, and resume

`ego` creates durable work. `--session NAME` creates or attaches a named session
within the canonical workspace; `--session-id ID` can create a caller-chosen
identity. `ego resume ID` requires an existing session. `--ephemeral` is refused.
The default store is `$EGO_HOME/sessions`; `--session-dir` selects another store.

```sh
ego ls --output json
ego usage --json
```

Listings are ordered by recency with a stable ID tie-breaker. `--all` spans
workspaces. Promptless `ego resume` uses the interactive picker; an explicit ID
opens that session. A resume carrying a prompt or `-` stays headless.
To keep a human approval seat, run without a prompt and enter the follow-up in
the composer:

```sh
ego resume ID --model openai-codex/gpt-6.1-sol --mode auto --sandbox workspace
```

A configured model overrides the saved session model on resume. Current startup
uses the saved model only when neither an explicit nor a configured model exists;
resume does not reliably keep the original model. The status line can show only
`gpt-6.1-sol`, but `--model` requires the full `openai-codex/gpt-6.1-sol` slug.

Interactive resume replays recent durable conversational turns. `/sessions`
browses sessions; `/history` exposes the conversational-history entry point.
History is derived from durable records, not a second authority for execution.

## Name and goal

Use `/name` to inspect/set the durable display name and `/goal` to manage the
persistent goal lifecycle. Headless metadata uses `ego resume ID --name ...` or
`ego resume ID --goal-operation ...`; use its metadata help for exact operations. A metadata
change requires an explicit session ID and does not submit a provider prompt.

## Fork

`ego fork SOURCE` branches at the inactive tip. `--at ITEM_ID` selects the
successful completed turn containing a durable user/assistant message, including
its full answer. Unknown/foreign IDs, incomplete/failed turns, and checkpoints
are refused. Inherited messages branch at their owning ancestor's boundary.
A selected completed boundary can be read while source work is active; a tip
fork needs an inactive source. `/fork [message-id]` uses the same service.

A fork retains lineage and conversation; it does not duplicate charges or live
run state. Instructions captured for the prompt epoch are inherited; `/refresh`
opens a new epoch when idle. Current tool admission remains current.

## File checkpoints

File checkpoints are recovery points for session-owned edits, not conversation
forks or compaction checkpoints. Built-in `write` and `edit` capture them in Git
worktrees. Preview the selected session before choosing a restore:

```sh
ego checkpoint SESSION_ID preview
ego checkpoint SESSION_ID restore CHECKPOINT_COMMIT
```

Preview reports checkpoint IDs, affected paths, and external-change conflicts.
Restore refuses conflicting external changes instead of forcing an overwrite.
`ego checkpoint SESSION_ID close` releases the session's private recovery ref
without restoring files. Do not close it while you still need those recovery
points. These are explicit operator actions, not model-generated prompts.

## Compact

`ego compact SOURCE` and idle-only `/compact` ask a tool-less generation for a
continuation checkpoint and create a new durable successor. Old conversation
items are not copied into the successor's active context; the checkpoint is a
separate context block. The source remains independently resumable.

The receipt identifies source/target and publication state. `--request-id UUID`
is an advanced idempotency override: reuse it only to recover the same transition
after a lost reply. A matching already-published target is inspected rather than
charged for another generation. A collision refuses without a new effect.
Compaction charges its source. Successor occupancy remains unknown until measured
at its next generation; checkpoint length is not a fresh baseline measurement.

## Export and usage

`ego export SOURCE` supports `md`, `json`, and `jsonl`, with optional `--file PATH`
and `--sanitize`. Local export can contain sensitive conversation and tool bytes.
Sanitized export structurally removes protected content; it does not make arbitrary
model prose safe to publish without review. Interactive `/export` defaults to
sanitized Markdown; explicit `local` selects local content.

`ego usage` reads existing accounting; `--period all|Ns|Nm|Nh`, `--all-roles`, and
`--json` control its view. Main, judge, compaction, hook, and child work can incur
separate accounting. Token usage, context occupancy, and cost are separate facts.
Subscription generations show explicitly labelled standard API equivalents, not
monetary zero. Usage JSON v4 keeps these in `subscription_equivalent`, separate
from monetary `known` totals; `subscription_unknown` records missing estimates.
Unknown or subscription monetary cost is not a known zero.

Busy writer leases, pending questions, held/paused sessions, and uncertain effects
can block transitions. Keep the recorded reason and use the appropriate
continuation rather than creating another request to disguise it.

Sources: [catalogue](../../crates/ego-cli/src/catalogue.rs),
[session identity](../../crates/ego-cli/src/session.rs),
[durable run controller](../../crates/ego-core/src/run.rs),
[usage](../../crates/ego-cli/src/usage.rs), and
[resume tests](../../crates/ego-cli/tests/resume_runtime.rs).
