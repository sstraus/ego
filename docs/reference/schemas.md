# Event and usage schemas

Two machine-readable outputs carry their own version number:

| Output | Produced by | Current version | Version field |
| --- | --- | --- | --- |
| Public event stream | `ego run --output jsonl`, `ego resume --output jsonl`; embedded in [local transcripts](transcript.md) | 21 | `v` on the `session_started` event |
| Usage report | `ego usage --json` | 6 | `v` at the top level |

Each version describes a shape. Check the version before you interpret the
payload, skip event types you do not know, and ignore fields you do not know.
Detect features by name from `session_started.capabilities`, not by comparing
version numbers. The durable session files under `$EGO_HOME/sessions` are a
private storage format, not this public stream; read sessions through
`ego export` instead.

## Public event stream (version 21)

JSONL output writes one JSON object per line, flushed as it happens. Every
object has a `type` field in `snake_case`. Internal and secret-bearing events
are never published. Identifiers (`session`, `turn`, `item`) are UUID strings.

### Event types

| `type` | Fields | Meaning |
| --- | --- | --- |
| `session_started` | `v`, `session`, `capabilities`; optional `mcp_errors`, `hook_errors`, `instructions` | Header event. `v` is the schema version. `capabilities` lists feature names such as `tools`, `instructions`, `schema`, `hooks_off` and `jsonl`. The optional lists appear only when non-empty |
| `turn_started` | `turn` | A turn began |
| `metering_selected` | `turn`, `metering`, `ceiling`, `rates` | How the generation about to run is billed, per wire attempt. `metering` is `currency`, `subscription` or `unmetered`. `ceiling.effect` is `none`, `binds` (with `limit` and optional `clamped_from`) or `inapplicable` (a monetary ceiling that cannot bind this row) |
| `generation_profile_selected` | `turn`, `item`, `profile` | The actual profile of an admitted generation. `item` is the parent task item for a foreground child, otherwise null |
| `provider_retry` | `turn`, `attempt`, `limit`, `recovery_remaining_ms`, `delay_ms`, `kind`, `capacity`, `cause` | A transient provider failure will be retried after `delay_ms`. Live only, never replayed |
| `provider_retrying` | `turn`, `attempt` | The announced wait ended and the next request is dispatching. Live only |
| `fallback_decision` | `turn`, `candidate`, `decision` | A fallback model was sanitized or skipped before any network effect |
| `item_started` | `turn`, `item`, `kind`; optional `name`, `text` | An item began. `name` is the tool name for `kind: "tool"`; `text` carries tool arguments or a permission description |
| `item_delta` | `turn`, `item`, `kind`, `text` | An incremental preview of an item's content |
| `item_completed` | `turn`, `item`, `kind`, `ok`; optional `text` | An item finished. Replace the preview with the completed text |
| `usage_updated` | `turn`, optional `item`, `usage_delta`, `cost_delta` | Token usage and cost of one generation, never a running total |
| `context_window_updated` | `turn`, `used_tokens`, `provenance` | Context occupancy changed. `provenance` is `generation_boundary`, `authoritative_query` or `local_estimate`, so a consumer can weigh the figure |
| `cost_anomaly` | `turn`, `model`, `estimated_upper_bound`; optional `cause` | The cost of a generation could not be established. An estimate is not a billed cost |
| `plan_updated` | `turn`, `update` | A durable worklist operation |
| `child_progress` | `turn`, `item`, `activity`, `output_tokens` | Transient activity of a child task, keyed by its parent task item. `output_tokens` is the child's cumulative output |
| `child_lifecycle` | `turn`, `item`, `child`, `phase`; `outcome` when settled | A child task started or settled. `child.kind` is `durable` or `foreground`; `phase` is `started` or `settled`; `outcome` is `completed`, `failed`, `interrupted` or `cancelled`. Carries no task text |
| `session_compacted` | `source`, `source_seq`, `target` | A headless run compacted `source` and continues in `target`, whose own `session_started` follows |
| `filter_notice` | `turn`, `text` | A transient diagnostic from a [protocol filter](../integrations/protocol-filter.md) |
| `error` | `turn`, optional `item`, `message`, `kind` | An error. `kind` is always present and is one of `transient`, `idle_timeout`, `context_overflow`, `invalid`, `policy_rejected`, `local_limit`, `refused` or `unclassified` |
| `turn_completed` | `turn`, `ok`, `exit_reason` | A turn ended. The last `turn_completed` in the stream is the terminal event |

`kind` on item events is one of `context_checkpoint`, `user_message`,
`agent_message`, `reasoning`, `tool`, `permission`, `question` or `subagent`.

`exit_reason` on `turn_completed` uses the turn vocabulary: `Completed`,
`Paused`, `Budget`, `Usage`, `PermissionRequired`, `ContextOverflow`,
`Provider`, `Guard`, `Internal`, or `{"Cancelled": "User" | "Timeout" | "Host"}`.
It explains one turn; the process exit code explains the whole run (see
[exit codes](README.md#output-and-exit-codes)).

`usage_delta` has the token fields described under
[token usage](#token-usage). `cost_delta` has a `cost` tag: `known` with an
`amount` and its `attribution`, or `unknown` with the reason. An unknown cost is
never zero.

### Version history

| Version | Change |
| --- | --- |
| 6 | `usage_delta` gained `cache_write_extended_tokens` and `reasoning_tokens`; `error` gained `kind` |
| 7 | Public question items and their structured report |
| 9 | The question target in the public report |
| 10 | `fallback_decision` |
| 11 | `provider_retry` |
| 13 | Optional goal outcome report |
| 14 | `provider_retrying` |
| 15 | `plan_updated` |
| 16 | `child_progress` |
| 17 | `cost_anomaly` |
| 18 | `cost_anomaly.cause` |
| 19 | `child_lifecycle` |
| 20 | `session_compacted` |
| 21 | Worklist suspension `reason` fields and host-only `restore` seeds carrying monotonic `started` IDs |

## Usage report (version 6)

`ego usage --json` prints one JSON object. Version 6 added current
per-credential subscription quota (`rate_limits`). Older report versions are
not read through compatibility defaults. Money amounts are decimal strings, not
JSON numbers, so they keep their exact value. Timestamps are Unix milliseconds.

| Field | Meaning |
| --- | --- |
| `v` | Report version, `6` |
| `as_of` | When the report was built |
| `period` | `{"kind": "all"}` or `{"kind": "last", "seconds": N}`, from `--period` |
| `sessions` | One entry per reported session (below) |
| `totals` | Accounting lines summed over every reported session |
| `provider_limits` | Present only when non-empty: current provider-stated limits for the active key, read without generating |
| `rate_limits` | Current subscription quota per stored credential (below) |

### Session entry

| Field | Meaning |
| --- | --- |
| `session` | Session UUID |
| `model` | The session's current main model, or null |
| `lineage` | Present for a successor: `{"kind": "fork", "source", "at_seq"}` or `{"kind": "compaction", "source", "source_seq", "prompt_revision"}` |
| `context` | Present when known: `used_tokens` and their `provenance` |
| `context_capacity` | Present when a generation stated it: `tokens`, `source`, `observed_at` |
| `provider_limits` | Present when a session response carried them: `provider`, optional `model`, `observed_at`, `limits` |
| `prompt_cache` | Present when a deadline is known: `model`, `ttl_seconds`, `expires_at`, and an optional `estimate` when the lifetime was learned rather than stated |
| `context_attribution` | Estimated context contribution per tool (`tools`, sorted by size), with the estimation `source`, `provenance` and `scope`, and `unmatched_results` |
| `lines` | The accounting lines for this session |

A provider limit has `kind` (`requests`, `tokens`, `input_tokens`,
`output_tokens` or `usd`), `remaining`, optional `limit`, `used` and `reset`,
and `source`. These are levels the provider stated; ego never subtracts or
predicts them.

### Accounting lines

`lines` and `totals` both have the fixed keys `main`, `judge`, `goal_check`,
`compaction`, `subagent`, `hook` and `total`. Each line has:

| Field | Meaning |
| --- | --- |
| `usage` | Token usage (below) |
| `cost.known` | Exact totals per currency |
| `cost.unknown` | Every reason a generation could not be priced |
| `cost.subscription_equivalent` | Display-only standard API equivalents of subscription work, never a monetary charge |
| `cost.subscription_unknown` | Subscription or unmetered work with no equivalent estimate |
| `cost.estimated` | Present only when non-empty: read-time estimates for generations counted in `unknown`, each with its `basis` (request-time or current price table) and `amount`. Never persisted and never counted toward a budget |
| `cost.estimate_unavailable` | Present only when non-empty: why an unknown generation could not be estimated |
| `cache_hit` | Present when the line has input tokens: cache reads as a fraction of all input tokens |

### Token usage

| Field | Meaning |
| --- | --- |
| `uncached_input_tokens` | Input billed at the full rate |
| `cache_read_tokens` | Input served from the provider's prompt cache |
| `cache_write_tokens` | Input written to the cache at standard retention |
| `cache_write_extended_tokens` | Input written to the cache at an extended retention |
| `output_tokens` | Generated tokens, including reasoning billed as output |
| `reasoning_tokens` | Present only when the provider stated it: the part of `output_tokens` attributed to reasoning. Absent means unknown, not zero |
| `web_search_requests` | Present only when non-zero: provider-billed searches |

The four input categories do not overlap.

### Subscription quota

Each `rate_limits` entry has `provider`, an optional `connection` (named
account ID), `credential` (a SHA-256 fingerprint, never the secret), `windows`,
`plan`, `credits`, `observed_at` and `source` (`passive` when seen on a response,
`active` when read). A window has `name` (such as `five_hour`, `seven_day` or
`monthly`), `used_percentage`, `resets_at` and `window_seconds`. `credits`, when
present, has `has_credits`, `unlimited` and `balance`. `ego auth usage --json`
reads quota without generating a model response.

Sources: [public events](../../crates/ego-core/src/public/mod.rs),
[child lifecycle](../../crates/ego-core/src/public/child.rs),
[usage report](../../crates/ego-core/src/usage.rs),
[token usage](../../crates/ego-core/src/state.rs), and
[quota](../../crates/ego-core/src/quota.rs).

Version 21 adds optional suspension `reason` values to worklist rows and step
updates, and the host-only `restore` worklist seed operation. Restore carries
`steps` plus the monotonic `started` ID set through fork and compaction. It is
not a model-callable operation. New model updates use the strict five-key
worklist contract in [CLI.md](../../CLI.md); recorded events are folded facts,
not fresh requests to admit. Live execution ownership is process-local and is
never reconstructed from seed history or an open recorded tool call.
