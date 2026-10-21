# Tools

Tools let a model perform work. Each tool prepares canonical operations; policy
permits those operations before a guarded port executes them. Listing a tool or
including it in `--tools` does not grant access.

Use `/tools` inside a session to inspect its current catalogue. `--tools` restricts
built-ins; `--no-tools` offers none. Skills, MCP attachments, and orchestration
can add admitted operations, subject to the same ceilings.

## Built-in effect tools

| Tool | Input | Purpose and boundary |
| --- | --- | --- |
| `read` | `path` | Read one canonical file; it cannot infer authority from a directory listing |
| `glob` | `pattern`, optional `path` | Find names, not file contents; root defaults to working directory |
| `grep` | `query`, optional `path` | Search contents under a bounded traversal; root defaults to working directory |
| `write` | `path`, `contents` | Create/replace a file with durable replacement semantics; parent must exist |
| `edit` | `path`, `old_string`, `new_string`, optional `replace_all` | Replace one unique literal match, or all original non-overlapping matches when explicitly requested; requires read and write authority |
| `exec` | `argv`, optional `cwd`, `timeout_ms` | Execute exact argv with admitted environment, confinement, deadline, and process-tree ownership |
| `read_tool_result` | `reference`, `offset` (default 0) | Page retained session evidence instead of treating a truncated preview as the full result |

Traversal skips `.git`, `target`, and `node_modules` and respects ignore rules.
These are usability limits, not security boundaries. Incomplete traversals state
that their output was truncated. Symlinks/canonical paths are checked at
preparation and guarded execution. A tool cannot replace the prepared target
with a wider one after permission.

`edit` requires an existing UTF-8 file and a nonempty `old_string`. By default,
that text must occur exactly once. Set `replace_all=true` for one substitution
across every original non-overlapping occurrence. Inserted text is not searched
again. An empty `new_string` deletes the matched text. Missing matches fail
without writing; this is not a batch of different substitutions.

All-match edits limit both input and output to 16 MiB. The permission preview
states the replacement scope without reading the file; the successful result
reports the actual occurrence count and line-change statistics from execution.
The same-file write allow can cover the supporting read, but explicit read or
write denies still refuse the operation. Use `write` to create a file or replace
its complete contents.

`exec` does not implicitly interpret a shell string. An explicit interpreter in
argv is a process request with its own risk and confinement. A timeout is not
proof that a child never started. Detached-process recovery also uses evidence
and settlement; do not repeat an uncertain effect blindly.

## Other model operations

| Operation | Purpose |
| --- | --- |
| `AskQuestion` | Suspend for a typed human answer; headless emits a question and exits 3 |
| `Skill` | Invoke admitted named instructions or read their admitted resources |
| `task` | Run an isolated depth-one child with inherited tools, permission, sandbox, offline posture, and shared budget; the child cannot delegate |
| `history.search`, `history.open` | Read bounded prior user/assistant conversation through the store-bound history port; exclude tool payloads |
| `memory_lookup` | Coordinator-profile-only lookup of decision/problem/handoff MDKB entries; absence/refusal is explicit |
| Collapsed MCP operations | Discover and call external tools from one captured admitted catalogue |
| `orchestrator_status`, `orchestrator_brief` | Observe native TUICommander children when its socket exists |
| `orchestrator_active` | Opt-in guarded coordinator actions; see [orchestration](../integrations/orchestration.md) |

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
`history.search` accepts `query`; `history.open` accepts a session UUID. History
is read-only conversational access across workspace roots, not arbitrary paths
or authority to execute a prior tool call. Native TUICommander orchestration is
a different operation from the in-process depth-one `task` child.

Sources: [task contract](../../crates/ego-core/src/turn/task.rs),
[history contract](../../crates/ego-core/src/history.rs),
[built-in registry](../../crates/ego-tools/src/tools/mod.rs),
[parameter schemas](../../crates/ego-tools/src/tools/),
[rule preparation](../../crates/ego-cli/src/rules.rs), and
[tool preparation/execution](../../crates/ego-core/src/permit/mod.rs).
