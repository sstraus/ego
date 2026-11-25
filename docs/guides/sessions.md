# Sessions and history

Read this page in the [hosted user guide](https://ego.tuicommander.com/docs/guides/sessions.html).

Durable sessions let you continue work after a terminal exits, inspect what
happened, or branch without overwriting the source. Terminal state and durable
conversation state are different: clearing the composer does not erase history.

**Sessions are deleted automatically after 30 days without use.** When a run
starts, at most once a day, ego permanently deletes in the background every session that
nobody opened or wrote for `sessions.retention_days` days (default 30), with
its search-history rows. A session that is open, held, or resumed within that
period stays, and so does every session that a kept fork or compacted session
starts from. To keep sessions longer, write a larger number of days in the user
configuration file, `$EGO_HOME/config.toml` (default `~/.ego/config.toml`);
there is no setting that turns deletion off:

```toml
[sessions]
retention_days = 365
```

Only the user configuration sets this key; a project file cannot change it.
`ego rm --expired --yes` runs the same deletion immediately and prints each
deleted session and the space freed.

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

The resume picker and `/resume` show this workspace's newest ten readable
sessions first. Press Page Down to load the next ten. Ordering uses the last
write time of `events.jsonl`, with session IDs in descending order for ties;
this last-use order can differ from `ego ls`, which uses recorded event times.
Filtering applies to the current page. Unreadable manifests or logs and
sessions still being created are skipped silently and do not use a page slot.
Listing does not repair files, acquire a writer lease, or write to the store.
Pages load away from terminal input: you can type a filter, press Escape, or
enter `/quit` while a page loads. A selected session with a held writer lease
is refused and leaves the current prompt usable. Headless resume without an
ID uses the same ordering but reads only the first readable workspace session.
To keep a human approval seat, run without a prompt and enter the follow-up in
the composer:

```sh
ego resume ID --model openai-codex/gpt-6.1-sol --mode auto --sandbox workspace
```

Three resume options cover scripts and changed settings:

- `ego resume --title TITLE [PROMPT]` (also `ego continue`) resumes the
  session of this workspace whose title, set with `/name` or `--name set=TEXT`,
  is exactly TITLE (case-sensitive). No match, or several sessions with that
  title, exits 2 without resuming; the error lists the matches, most recent
  first.
- `ego resume ID --queue PROMPT` (also `-f FILE` or `-` for stdin) waits while
  another process owns the session (an interactive session, another resume, an
  ACP connection or an earlier queued prompt), then runs the prompt headless
  with the same admission as `ego resume ID PROMPT` and exits with that turn's
  status. Queued prompts on one session run one turn at a time, in no
  guaranteed order. The wait counts toward `--timeout` (exit 124). `--queue`
  needs a prompt and is refused with `--answer` and with profile `ci`.
- `ego resume ID --accept-authority-delta` accepts the change in execution
  authority that ego shows when the saved session's authority differs from
  the current configuration, for this resume only; ego records the decision
  and a later, different change asks again. Without it, a headless resume
  with wider authority exits 3 before any provider call. Administrator
  ceilings still apply.

A configured model overrides the saved session model on resume. Current startup
uses the saved model only when neither an explicit nor a configured model exists;
resume does not reliably keep the original model. The status line can show only
`gpt-6.1-sol`, but `--model` requires the full `openai-codex/gpt-6.1-sol` slug.

Interactive resume replays recent durable conversational turns. `/sessions`
browses sessions across workspaces in pages of ten; `/history` exposes the
cross-workspace conversational-history entry point with the same paged loader.
History is derived from durable records, not a second authority for execution.

## Name and goal

Use `/name` to inspect/set the durable display name and `/goal` to manage the
persistent goal lifecycle. Headless metadata uses `ego resume ID --name ...` or
`ego resume ID --goal-operation show|set=TEXT|achieve|resume|block=REASON`. A metadata
change requires an explicit session ID and does not submit a provider prompt.

## Fork, rewind, checkpoint, compact and export

Five commands touch a session's state and each does one thing:

| Command | Changes files | Changes conversation | REPL |
| --- | --- | --- | --- |
| `ego checkpoint ID preview\|restore\|close` | restores files that the session's `write` and `edit` changed | no | none |
| `ego fork SOURCE [--at ITEM_ID]` | no | copies the conversation at the tip or after a completed turn | `/fork` |
| `ego rewind SOURCE --at ITEM_ID` | restores later file edits, newest first | forks the conversation after that turn (a checkpoint restore followed by a fork) | none |
| `ego compact SOURCE` | no | publishes a compacted successor session | `/compact` |
| `ego export SOURCE` | no | no (read-only render) | `/export` |

## Rewind

`ego rewind SOURCE --at ITEM_ID` returns both the conversation and the files to
the turn that contains the selected user or assistant message. It selects the
turn as `ego fork --at` does and needs an inactive session. It finds the file
checkpoints that the session's built-in `write`, `edit` and `notebook_edit`
results recorded after that turn, restores them newest first, and then creates
the fork. Text output is the new session ID with one `restored PATH` line per
changed file; `--output json` lists every checkpoint with its path and effect.

Before it changes anything, rewind compares each file with the disk and with
every later write of it. It refuses, listing the files, when a file was edited
outside ego after that turn; keep or undo those edits first. It also refuses
when a later write saved no checkpoint (checkpoints are off, or the file is
outside Git), when a `task` child wrote files after the turn, and when a
recorded checkpoint is missing from the repository. Commands run through
`exec` and MCP effects are not captured, so rewind does not undo them.

The source session is not changed. If a restore fails part-way, the earlier
restores stay applied and no session is created; running the same rewind again
is safe. Use `-C` for the repository the session wrote to.

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

Headless `ego run` and `ego resume` compact automatically only when you turn it
on with `ego config set compaction.headless=true` (or its row in `ego setup`);
only the user configuration sets it. Off, a turn that ends with
`context_overflow` stops the run with exit 4. On, the run compacts the source
and continues in the successor in the same process. The event stream carries
`{"type":"session_compacted","source":"…","source_seq":N,"target":"…"}`,
followed by the successor's `session_started`, and the final summary names the
successor in `session`. The run budget, deadline and `--max-turns` cover both
sessions. A failed compaction keeps exit 4 and names `ego compact SOURCE` as
the retry.

## Model-requested context changes

In a durable interactive session, the model can ask to compact the
conversation or to clear it with the `context` tool, giving a reason and a
handoff note. The request and its reason appear in the transcript. Ego waits
until the current tool batch and pending operator work are done, then
publishes a successor session and continues there without a new prompt.
`compact` writes the usual checkpoint. `clear` writes no checkpoint and keeps
only the active goal with its limits, fresh instructions and the model's
handoff note.

| Option | Effect |
| --- | --- |
| `--context-clear` | Allows `clear`, which also needs an active goal; otherwise a clear request is refused |
| `--context-interval N` | Minimum completed turns between two model-requested changes (default 10, N ≥ 1) |
| `--no-context-tool` | Removes the tool; automatic compaction under context pressure still runs |

The tool is also absent when `--tools` selects an explicit tool list. Headless
runs and ACP sessions do not offer it; their explicit compaction commands are
unchanged.

## Delete sessions

`ego rm ID` deletes one session from the listings. Without `--yes`, it asks for
confirmation on the terminal. Running, leased or held sessions are refused.
History that a fork descendant needs is retained.

`ego rm --expired --yes` runs the 30-day retention pass described at the top of
this page now. `ego rm --incompatible --yes` permanently removes sessions whose
stored format this version cannot read, and keeps readable sessions; listings
show a count of such sessions instead of one error per session.

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
monetary zero. Usage JSON (version 6) keeps these in `subscription_equivalent`,
separate from monetary `known` totals; `subscription_unknown` records missing
estimates. Unknown or subscription monetary cost is not a known zero. The
[schema reference](../reference/schemas.md) describes the usage JSON and the
headless event stream.

Busy writer leases, pending questions, held/paused sessions, and uncertain effects
can block transitions. Keep the recorded reason and use the appropriate
continuation rather than creating another request to disguise it.

Sources: [catalogue](../../crates/ego-cli/src/catalogue.rs),
[session identity](../../crates/ego-cli/src/session.rs),
[durable run controller](../../crates/ego-core/src/run.rs),
[usage](../../crates/ego-cli/src/usage.rs), and
[resume tests](../../crates/ego-cli/tests/it/resume_runtime.rs).
