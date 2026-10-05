# Sessions, runs, turns, permits, and evidence

These concepts explain what survives a restart, what a permission authorizes,
and why an uncertain effect must not be retried as though nothing happened.

| Term | Meaning | Why it matters |
| --- | --- | --- |
| Session | Durable conversation identity, ordered events, metadata, lineage, and captured context | You can resume it and trace its history independently of a terminal or host connection |
| Run | Admitted execution request, rooted in a session and governed by the run controller | Start, continue, cancel, and status share identity, visibility, revisions, and settlement |
| Turn | One accepted prompt and its generations/tool work until settlement | Retries and fallbacks remain part of the same turn rather than new user work |
| Generation | One provider attempt under current model, budget, tools, and context | Catalogue display is not permission to send; facts resolve again at this boundary |
| Prepared operation | Canonical effects derived from tool arguments before execution | Policy judges actual targets rather than model prose |
| Permit | Authority bound to the prepared effects and policy snapshot | Execution cannot enlarge the operation after approval |
| Evidence | Durable records and retained blobs of admitted inputs, decisions, execution, and settlement | A preview, a printed frame, and proof of an effect are different facts |
| Trust | Permission to load project configuration and ambient instructions | Trust is separate from tool authority and sandbox strength |
| Confinement | The operating system's demonstrated boundary for effects | A mode called `yolo` cannot raise that boundary |

```text
accepted prompt → admitted run → turn → generation → prepared tool call
                                                   ↓
                                   policy → permit → guarded execution
                                                   ↓
                                         evidence → settlement
```

## Uncertain runs

`RunStatus` includes `running`, `cancelling`, `settled`, `paused`, `cancelled`,
`interrupted`, and `uncertain`. `uncertain` means the process cannot prove whether
the turn ran. It is not a successful completion and not proof that no effect
occurred. A crash, ambiguous delivery, or post-publication durability failure
can leave a result that must be inspected before further effects.

The run controller retains typed settlement and diagnostics. Recovery uses
recorded evidence rather than blindly resending work. Lifecycle transitions can
refuse an uncertain session. Inspect the host's run status and retained evidence;
do not create a new request just to hide uncertainty.

Configuration and compaction can also publish with durability uncertainty. The
new authority or successor already exists after publication; a warning does not
mean it is safe to retry, roll back, or delete it. Their receipts distinguish
not published, published durably, and published with durability uncertainty.

## Context and accounting

A fork inherits conversation at a durable boundary. Compaction instead asks a
model for a checkpoint and creates a new session with that checkpoint; the
source remains resumable. Usage belongs to the source that incurred it.
Subscription, local unmetered, unknown monetary cost, and a priced zero are
separate facts. A token count does not imply a known money total.

Sources: [run controller](../../crates/ego-core/src/run.rs),
[permit module](../../crates/ego-core/src/permit/mod.rs),
[events](../../crates/ego-core/src/event/mod.rs), and
[sessions and history](../guides/sessions.md).
