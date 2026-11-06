# Local transcript v1

`ego export SESSION --format transcript` writes UTF-8 JSONL to stdout. This is
an explicit local-content export, not a sanitized audit bundle. It preserves
prompts, answers, reasoning text, tool arguments/results and checkpoint text.
Known environment secret values are masked by the same filter as other local
exports. Opaque provider content, reasoning continuation tokens, credentials,
private tool correlation tokens and internal evidence are not exported.

## Records

The first line has `type: "transcript.header"`, `v: 1`, `session` (the requested
session UUID), `public_event_version` (the embedded public event schema),
`lineage` and `compaction_predecessor`. Both latter fields are null for a root.
A fork lineage is `{ "kind": "fork", "source": UUID, "at_seq": N }`.
A compaction lineage is `{ "kind": "compaction", "source": UUID,
"source_seq": N, "prompt_revision": N }`; `compaction_predecessor` is its source
UUID. These describe the immediate predecessor, not a flattened conversation.
Export each predecessor separately if older conversation is wanted. A fork
inherits its source through `at_seq`; a compaction starts clean with a public
`context_checkpoint` item, not duplicated ancestor chat.

Each remaining line has `type: "transcript.event"`, `session`, `id` (stable
durable event UUID), `seq` (session-local ordered integer), `turn` (UUID), `item`
(UUID or null), `ts` (UTC milliseconds since the Unix epoch), and `event` (the public payload).
The pair `(session, id)` identifies an event across repeated exports. Sequence
gaps are normal: internal events are omitted. Sequence, not timestamp, orders
records. Never use item identity to deduplicate different events.

`event` reuses ego's public projection (`CLI.md`, public JSONL output):
`item_started`, `item_delta`, `item_completed`, `turn_completed`, usage and
other public facts. A completed `user_message` holds the user prompt; a completed
`agent_message` holds the full answer. Deltas are incremental previews: replace
the preview with completed text, do not append the full answer to the deltas.
Tool items use the same item UUID for start (name and arguments), deltas and
completion (ok and result text). Checkpoints are `item_completed` with
`kind: "context_checkpoint"`. Live-only harness announcements are not replayed.

The local transcript adds these public, explicitly selected goal payloads:

- `goal_set`: `id`, `objective`, `owner`, `derived_from` (nullable goal UUID).
- `goal_updated`: `id`, `objective`.
- `goal_continuation`: `id`, `text`. This is system-owned work, **not user input**.
- `goal_transitioned`: `id`, `transition` (the structured goal observation).

The payload's goal `id` is distinct from the outer durable event `id`.
Goal owner and transition use the existing serialized goal vocabulary.

## Follow and reconnect

`ego export SESSION --format transcript --follow` emits the header and current
snapshot, then polls committed session entries every 250 ms and flushes new
records to stdout. It never takes the session writer lease. Stop the process to
stop following; it intentionally stays alive when a turn ends or the writer
exits, since the same session can resume. A broken stdout terminates the command.

`--after N` omits event records with `seq <= N`, but always emits the header.
It works for both snapshot and follow. Save the last completely parsed event's
sequence for reconnect, qualify it by header session, and deduplicate by event
identity if needed. There is no end marker. Ignore an incomplete final output
line after process failure. Missing/corrupt sessions fail nonzero; diagnostics
are on stderr, not mixed into JSONL. A reconnect does not fabricate lost events.

Follow stays on the requested session: it does not silently follow a fork or
compaction into a new identity. When the terminal's ego session binding changes,
start a new export for that session and inspect its header. The successor's
header names the predecessor and its checkpoint event carries the new context.
This avoids mixing branch histories. Readers need no private store paths.

`--follow` and `--after` require `--format transcript`; transcript mode cannot
be combined with `--sanitize` or `--file`. Use stdout redirection to save a
snapshot. The command is local-only and requires no model or service access.

## Compatibility and fixture

Readers must skip unknown record and payload kinds and ignore added fields.
Reject unsupported header versions instead of guessing semantics. New optional
fields and new kinds are additive within v1; incompatible interpretation changes
require a new transcript version. The embedded public schema has its own version.

`crates/ego-cli/tests/fixtures/transcript-v1.jsonl` is recorded from the real CLI
against the scripted loopback provider by the transcript contract test. It
contains the root snapshot and compaction-successor export, each with its own
header. Between these snapshots the test observes a new operator goal through
follow; the successor therefore derives from that newer goal, not the older goal
in the root snapshot.
The capture procedure and assertions are in `crates/ego-cli/tests/it/transcript.rs`.
No paid API or live external service is used.
