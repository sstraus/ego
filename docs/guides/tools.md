# Tools

Tools let a model perform work. Each tool prepares canonical operations; policy
permits those operations before a guarded port executes them. Listing a tool or
including it in `--tools` does not grant access.

Use `/tools` inside a session to inspect its current catalogue. `--tools` restricts
built-ins; `--no-tools` offers none. Skills and MCP attachments
can add admitted operations, subject to the same ceilings.

## Built-in effect tools

| Tool | Input | Purpose and boundary |
| --- | --- | --- |
| `read` | `path` | Read one canonical file; it cannot infer authority from a directory listing |
| `glob` | `pattern`, optional `path` | Find names, not file contents; root defaults to working directory |
| `grep` | `query`, optional `path` | Search contents under a bounded traversal; root defaults to working directory |
| `write` | `path`, `contents` | Create/replace a file with durable replacement semantics; parent must exist |
| `edit` | `path`, `old_string`, `new_string`, optional `replace_all` | Conditional literal replacement; requires both read and write authority |
| `notebook_edit` | `path`, cell edit operation | Replace, insert, or delete one nbformat 4.5 cell by stable ID; preserve other cells and do not execute code |
| `exec` | `argv`, optional `cwd`, `timeout_ms`, `background` | Execute exact argv with admitted environment, confinement, deadline, and process-tree ownership |
| `exec_status`, `exec_cancel` | `id` | Inspect or cancel a background process owned by this session |
| `web_fetch` | `url` | Fetch bounded public HTTP(S) text without URL credentials; redirects require a fresh admitted call |
| `web_search` | `query`, `allow_paid` | Provider-hosted search or explicitly admitted Exa search, depending on the active provider and configuration |
| `read_tool_result` | `reference`, `offset` (default 0) | Page retained session evidence instead of treating a truncated preview as the full result |

Traversal skips `.git`, `target`, and `node_modules` and respects ignore rules.
These are usability limits, not security boundaries. Incomplete traversals state
that their output was truncated. Symlinks/canonical paths are checked at
preparation and guarded execution. A tool cannot replace the prepared target
with a wider one after permission.

`edit` requires an existing UTF-8 file and rejects unknown input fields. By default,
`old_string` must occur exactly once. Set `replace_all=true` for all original
non-overlapping matches of one nonempty string. Inserted text is not searched
again. An empty `new_string` deletes the matched text; missing matches fail
without writing. All-match edits limit both input and output to 16 MiB.
The permission preview states the scope without reading the file. Successful
results report actual occurrence and changed-line counts. Bulk edit transcript
rows show the execution receipt, not statistics inferred from one fragment.
Use `write` to create a file or replace its complete contents.

`exec` does not implicitly interpret a shell string. An explicit interpreter in
argv is a process request with its own risk and confinement. A timeout is not
proof that a child never started. Detached-process recovery also uses evidence
and settlement; do not repeat an uncertain effect blindly.

A foreground `exec` call has a default lifetime of two minutes. `timeout_ms`
accepts 1 through 600000 (ten minutes); omitted or null uses the default. The
transcript row shows the effective limit, for example `(timeout 2m)`. For longer
work, use `background=true`: then an omitted or null `timeout_ms` means no
deadline. A foreground command promoted with Ctrl+B keeps its original limit.
On expiry, ego terminates the child and its descendants and reports a timeout.
Time while ego is suspended does not count.

## Background processes and web access

`exec` with `background=true` returns a session-owned ID. The model can use
`exec_status` to inspect bounded output and settlement, or `exec_cancel` to cancel
that process. Interactive users can inspect `/tasks` and cancel with
`/tasks cancel ID`. An ID is not permission to inspect another session's process.

`web_fetch` accepts public HTTP(S) URLs, at most 1 MiB of input and 32 KiB of
output, with a 30-second limit. It does not forward credentials or follow a
redirect under the original destination's authority. Each new destination needs
its own policy-checked call; public-address checks still apply.

`web_search` depends on the selected provider and admitted catalogue. Hosted
OpenAI/Anthropic search uses that provider's credential and can incur search and
token charges. Other providers use the Exa adapter only when configured, with
`EXA_API_KEY`, `allow_paid=true`, and ordinary network permission. There is no
silent fallback between backends. See [web search reference](../reference/tools.md).

## Other model operations

| Operation | Purpose |
| --- | --- |
| `ask_question` | Suspend for a typed human answer; headless emits a question and exits 3 |
| `skill` | Invoke admitted named instructions or read their admitted resources |
| `goal` | Set a model-authored session goal; cannot replace an active operator-owned goal |
| `plan` | Replace durable ordered steps or update one step; this does not achieve the session goal |
| `context` | Request a compact/clear transition after the current tool batch; clear needs operator opt-in and an active goal |
| `task` | Run an isolated depth-one child with inherited tools, permission, sandbox, offline posture, and shared budget; the child cannot delegate |
| `history.search`, `history.open` | Read bounded prior user/assistant conversation through the store-bound history port; exclude tool payloads |
| `memory_lookup` | Coordinator-profile-only lookup of decision/problem/handoff MDKB entries; absence/refusal is explicit |
| Collapsed MCP operations | Discover and call external tools from one captured admitted catalogue |

The available catalogue depends on session composition, admitted extensions,
provider capability, and configuration; these names are not a promise that every
session offers every operation.

For a deterministic policy explanation without execution:

```sh
ego rules explain read '{"path":"README.md"}'
```

The result explicitly reports `would_execute=false`. `rules remember` derives
predicates but can lose invocation details the grammar cannot express; preview
its omissions before persisting an allow. See [permissions](permissions.md).

`task` accepts `name` and `prompt` and returns the bounded final child answer.
With `background=true`, it starts a durable child and returns its ID immediately;
parent-turn interruption does not cancel that child. Use `/tasks` to inspect or
cancel it. Background calls require an explicit prompt and cannot select `agent`
or `plan_step`. A foreground call can bind an existing durable `plan_step`.

Background exec settlement wakes an idle interactive session to process the
retained result; the model need not repeatedly request `exec_status` while waiting.
`history.search` accepts `query`; `history.open` accepts a session UUID. History
is read-only conversational access across workspace roots, not arbitrary paths
or authority to execute a prior tool call. Coordination through the TUIC API is
a different operation from the in-process depth-one `task` child.

Sources: [task contract](../../crates/ego-core/src/turn/task.rs),
[history contract](../../crates/ego-core/src/history.rs),
[built-in registry](../../crates/ego-tools/src/tools/mod.rs),
[parameter schemas](../../crates/ego-tools/src/tools/),
[rule preparation](../../crates/ego-cli/src/rules.rs), and
[tool preparation/execution](../../crates/ego-core/src/permit/mod.rs).
