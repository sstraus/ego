# Tools

Read this page in the [hosted user guide](https://sstraus.github.io/ego/guides/tools.html).

Tools let a model perform work. Each tool prepares canonical operations; policy
permits those operations before a guarded port executes them. Listing a tool or
including it in `--tools` does not grant access.

Use `/tools` inside a session to inspect its current catalogue. `--tools` restricts
built-ins; `--no-tools` offers none. Skills and MCP attachments
can add admitted operations, subject to the same ceilings.

## Built-in effect tools

| Tool | Input | Purpose and boundary |
| --- | --- | --- |
| `read` | `files`: one to sixteen `path` entries, each with optional byte `offset` | Admit every canonical target before reading; ordered per-file JSON results share a 32 KiB serialized budget |
| `glob` | `pattern`, optional `path` | Find names, not file contents; root defaults to working directory |
| `grep` | `query`, optional `path` | Search contents under a bounded traversal; root defaults to working directory |
| `write` | `files: [{path, contents}, …]` | Replace 1–16 files in order. Each parent must exist. All writes need admission before execution. Execution stops on the first error; earlier writes are not rolled back. |
| `edit` | `files: [{path, chunks: [{old_string, new_string}], replace_all?}, …]` | Conditional literal replacement; requires both read and write authority |
| `notebook_edit` | `path`, cell edit operation | Replace, insert, or delete one nbformat 4.5 cell by stable ID; preserve other cells and do not execute code |
| `exec` | `argv`, optional `cwd`, `timeout_ms`, `background` | Execute exact argv with admitted environment, confinement, deadline, and process-tree ownership |
| `exec_status`, `exec_cancel` | `id` | Inspect or cancel a background process owned by this session |
| `exec_monitor` | `id`, optional `pattern`, `max_lines` | Deliver new complete output lines of a background process owned by this session, without polling |
| `web_fetch` | `url`, `max_output_kib` | Fetch bounded HTTP(S) text without URL credentials from public addresses or owner-approved local or LAN addresses; follows up to 5 same-origin redirects, other redirects require a fresh admitted call |
| `web_search` | `query`, `allow_paid` | Provider-hosted search or explicitly admitted Exa search, depending on the active provider and configuration |
| `read_tool_result` | `reference`, `offset` (default 0) | Page retained session evidence instead of treating a truncated preview as the full result |

Traversal skips `.git`, `target`, and `node_modules` and respects ignore rules.
These are usability limits, not security boundaries. Incomplete traversals state
that their output was truncated. Symlinks/canonical paths are checked at
preparation and guarded execution. A tool cannot replace the prepared target
with a wider one after permission.

`edit` accepts 1–16 distinct existing UTF-8 files and 1–16 chunks per file.
All reads and writes need admission before execution. Each chunk matches the
original file, and chunk spans must not overlap. By default, each `old_string`
must occur exactly once. Set `replace_all=true` on a file with one chunk to
replace all original non-overlapping matches of its nonempty string. Inserted
text is not searched again. An empty `new_string` deletes the matched text.
Execution stops on the first failure; earlier file changes are not rolled back.
All-match edits limit both input and output to 16 MiB. Unknown input fields fail.

```json
{"files":[{"path":"src/example.txt","chunks":[{"old_string":"before","new_string":"after"}]}]}
```
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

`exec_monitor` subscribes the model to new complete output lines of that
process; see "Background output monitor" in `CLI.md` for its delivery and
bounds. It needs the same authority as `exec_status`.

`web_fetch` accepts public HTTP(S) URLs, at most 1 MiB of input and 32 KiB of
output, with a 30-second limit for the whole call. `max_output_kib` (1 to 256)
requests a larger page; the input limit is always 32 times the output limit, so
8 MiB at most. Ask the agent for a larger page when the result ends with
`[Content truncated at ...]`. It does not forward credentials.

A redirect to the same scheme, host and port is followed in the same call, up to
5 hops. Network rules match only host and port, so these hops use the decision
that already admitted the request. Each hop is a new GET with no headers, no
body and no credentials, and the address checks of that decision run again: the
public-address check, or the owner approval of a local or LAN address described
below. The result names the final `Source:` and the `Requested:` URL. A sixth
hop fails the call.

Any other redirect is not followed: another host (an apex domain to its `www`
host), another port, or `http` to `https`. The result gives the destination for
a new policy-checked call. A redirect from HTTPS to HTTP is refused. No setting
turns same-origin following off; a `net(host)` deny rule blocks the host
entirely.

By default `web_fetch` refuses loopback, private, link-local and shared-address
(Tailscale `100.x`) destinations. The owner opens them in one of three ways:

- Add a `net(...)` rule that names hosts to the **user** configuration allow
  list. Use the usual `net` vocabulary:
  - `net(localhost:3000)` or `net(192.168.1.20:8080)`: one host and one port.
  - `net(localhost)`: a host without a port covers every port, for example
    3000 and 5173.
  - `net(*.lan)`: every subdomain of `lan` (not `lan` itself).
  - `net(192.168.1.0/24)` or `net(fd00::/8)`: every address literal in a CIDR
    block. Add a port to limit it: `net(192.168.1.0/24:8080)`. An IPv6 block
    with a port needs brackets: `net([fd00::]/8:443)`. A CIDR block matches
    address literals only, never a host name.

  A name rule also admits the local address that the name resolves to,
  because the owner named it. Keep name patterns narrow: a broad public
  pattern such as `net(*.com)` would also admit a public name that resolves to
  a local address.
- Approve a prompt for an address literal such as `http://192.168.1.20:8080/`.
  Approving a prompt for a host name does not unlock private addresses.
- Set `web_private_network = true` in the user configuration, or select it in
  `ego setup`. This turns the block off for every local and LAN address of every
  admitted `web_fetch` call. The default is `false` (block on). Only the user
  configuration can set it; a project or local repository configuration that
  sets it is a configuration error.

An address outside the rule's pattern stays refused. Bare `net(*)` (or
`net(*:PORT)`), project and local repository rules and the permission modes
(`auto`, `yolo`) never unlock a private address. A redirect to another endpoint
is reported and not followed, and a name that no owner rule covers and that
resolves to a local address is refused. Unspecified, broadcast and multicast
addresses stay refused, also with `web_private_network = true`. Remove the rule
or the setting to revoke the approval.

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
| Collapsed MCP operations | Discover and call external tools from one captured admitted catalogue |

The available catalogue depends on session composition, admitted extensions,
provider capability, and configuration; these names are not a promise that every
session offers every operation.

For a deterministic policy explanation without execution:

```sh
ego rules explain read '{"files":[{"path":"README.md"}]}'
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
