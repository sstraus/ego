# Tools

Read this page in the [hosted user guide](https://ego.tuicommander.com/docs/guides/tools.html).

Tools let a model perform work. Each tool prepares canonical operations; policy
permits those operations before a guarded port executes them. Listing a tool or
including it in `--tools` does not grant access.

Use `/tools` inside a session to inspect its current catalogue. Skills and MCP
attachments can add admitted operations, subject to the same ceilings.

`--tools LIST` (or a saved `tools` selection) keeps only the tools it names.
Every other tool is removed, not only the file and process tools: `ask_question`,
`task`, `model_catalogue`, `goal`, `worklist`, `history.search`, `history.open`,
`skill`, `image_gen`, `web_search` and the opt-in `memory`, `lsp` and
`build_recipe` stay only when listed. Any list also removes `context`, which a
list cannot name. A name the run does not offer is refused with the names it
does offer. A non-empty list does not filter attached MCP tools. `--no-tools`
offers no tools at all; on `run` and `resume` it also disables MCP.

## Compact file paths

Tool rows show file paths relative to the active session workspace, so a file
inside the repository appears as `crates/ego-cli/src/output.rs` rather than
repeating the repository's absolute path. The workspace itself appears as `.`.
Paths outside the workspace use `~` for the current user's home when possible;
other absolute paths and already-relative arguments stay unchanged. Native path
components and separators are used on each operating system.

This is display-only, including transcript replay and in-process workspace
switches. Tool arguments, recorded evidence, file contents, search queries,
glob patterns, permissions and execution paths are not rewritten.

## Image-generation account boundary

The current subscription `image_gen` implementation reads a provider-wide
credential slot, not a named connection. It is offered only when the captured
run configuration has no named connections and selects none. Named-account runs
never borrow that unrelated identity. An explicitly configured `OPENAI_API_KEY` can
still provide the independent API image backend, which requires per-image paid
consent; without it, `image_gen` is absent in a named-account run. Named subscription
image access is not implemented. See [the current limitation](../design-limits.md#subscription-image-access-is-not-bound-to-named-accounts).

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
| `web_search` | Provider-native declaration | Search through the resolved model provider only; no independent fallback |
| `web_search_external` | `query`, `allow_paid` | Explicit independent Exa search with its own credential and network/cost consent |
| `read_tool_result` | `reference`, `offset` (default 0) | Read one fixed 32 KiB page of this session's complete process output instead of treating a truncated preview as the full result |
| `image_gen` | `prompt`, optional `path`, `size`, `quality`, `allow_paid` | Generate one raster image and write it as a new PNG file in the workspace; never overwrites a file. With `OPENAI_API_KEY` each image is charged to that account and needs `allow_paid=true`; see the account boundary above for the subscription backend. Needs network and write permission |

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
bounds. It needs the same authority as `exec_status`. If the process has already
settled, registration returns a normal no-new-output receipt; `exec_status` still
reads its output and outcome.

`web_fetch` accepts public HTTP(S) URLs with an independent 8 MiB download
limit and a 30-second limit for the whole call. Rendered output defaults to
32 KiB; `max_output_kib` (1 to 256) changes only the rendered-text limit.
A small output budget does not reduce the download limit. Ask the agent for
more output when the result ends with `[Content truncated at ...]`.
It does not forward credentials.

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

Native `web_search` defaults follow the resolved endpoint and protocol, not a
list of model names. See the [search reference](../reference/tools.md) for the
official routes. Compatible custom gateways stay off by default. OpenAI subscription defaults use cached search; explicit selection
retains live access. Claude and xAI native search use live access, with no cached mode.
Offering search does not prove account entitlement. Explicit `--tools web_search` enables native search on
capable API and OAuth routes without `EXA_API_KEY`; unsupported endpoints can
reject it. Each generation uses its resolved provider's
search capability, credential, and provider authority. Pricing follows the
selected route; API routes are not presumed free. An explicit `--tools` list that omits `web_search`, or
`--no-tools`, disables it. Defaults do not use Exa, even when `EXA_API_KEY`
is configured. `--tools web_search` remains native-only after a model change,
child invocation or resume; unsupported native search is omitted, not replaced.
Use `--tools web_search_external` for the independent Exa adapter, with
`allow_paid=true` and ordinary network permission. Existing configurations that
selected Exa through `web_search` must use the new explicit name. Native request
rejection does not retry through Exa. On the OpenAI subscription route, default requests set
`external_web_access=false`; explicit native selection sets it to `true`.
OpenAI subscription and xAI Responses do not send an individual-search limit.
See the [web search reference](../reference/tools.md) for the limits that other
routes can send.

## How output reaches the model

Tool results are bounded so that one large output cannot fill the context. The
complete output stays in the session; the model receives a shaped answer and,
where something was left out, a reference it can follow.

### Command output

Each output stream of an `exec` call (stdout and stderr) keeps up to 64 KiB for
the model. Longer output shows its start and its end, separated by
`[... N bytes omitted; read_tool_result REF ...]`. The complete bytes stay in the
session evidence.

Text output is then compacted:

- A carriage-return redraw counts as its last version, as a terminal shows it.
- Each run of build progress lines (lines that start with `Compiling`,
  `Checking`, `Downloading`, `Building [` and similar words) becomes its last
  line plus `[N earlier progress lines folded; the full output is recoverable]`.
- If the result is still longer than 4 KiB, the model receives only its last
  4 KiB, headed `[bounded output excerpt; earlier output omitted]`. Earlier
  warnings and errors can fall outside this excerpt.
- A compacted stream ends with `[complete output: read_tool_result REF]`.

A failed command is compacted the same way, and its exit status stays visible.
The tail of a background command in `exec_status` is compacted with the same
rules.

`read_tool_result` returns the complete output in fixed 32 KiB pages, by byte
offset. It reads process output of the current session only; the model copies
the reference from an earlier result.

### Learned build recipes

A build or test command that the agent runs again and again prints mostly the
same noise. A learned recipe lets the model replace that noise in later answers
with the lines that matter. Recipes are off by default. Turn them on with the
`build_recipes` row in `ego setup` or `ego config set build_recipes=true`. Only
the user configuration can set the key; a project or local configuration that
sets it is refused. `--bare`, or no `$EGO_HOME`, turns them off for one run.

When recipes are on, the model has the `build_recipe` tool. After it has seen
the output of a repeated command, it can save a recipe for that exact argv. A
recipe is data, not code:

- `argv`: the exact argument vector. Another argument, flag or order is another
  command, and the recipe does not apply to it.
- `require`: 1 to 8 literal markers that every later output must contain.
- `omit`: 1 to 16 literal line rules, `prefix` or `contains`. There is no
  pattern language.

The tool checks the recipe against output references of this session and saves
it only when every marker is present and the compact answer is smaller than the
output. It runs no process and cannot change what `exec` runs or what the
policy admits. Plan mode refuses it. Saving the same argv again stores a new
revision.

A later foreground `exec` of exactly that argv that exits 0 then gives the model
a compact answer instead of the usual output: a header with the recipe ID,
revision and number of omitted lines, then for each stream its complete-output
reference and every line no rule omits, at most 6 KiB of kept lines. A line that
contains `error`, `warn`, `fail`, `fatal`, `panic`, `exception` or `deprecat`
(in any ASCII letter case) is never omitted. The terminal shows the usual output. The usual answer is kept when the command fails,
is killed or runs in the background, when a marker is missing, when the output
is not UTF-8 or is larger than 4 MiB, when the kept lines exceed 6 KiB, when
the compact answer would not be smaller, or when the recipe is disabled. The
session records the answer that was sent, so a resumed session replays it
unchanged even after the recipe changes.

Enable recipes when the agent repeats the same long build or test commands in
one workspace. What it saves is context: each repeated successful run costs the
kept lines instead of the full build log, and the complete output stays one
`read_tool_result` call away.

Each workspace has one recipe book, `$EGO_HOME/build-recipes/NAME-HASH.json`,
readable only by its owner, with at most 64 recipes and no output bytes.
Recipes are never shared between workspaces. Manage the book of the current
directory with:

```sh
ego build-recipes ls            # ID, revision, state, sizes and argv of each recipe
ego build-recipes show ID       # one recipe as JSON
ego build-recipes disable ID    # stop using it; it stays stored
ego build-recipes enable ID     # use it again
ego build-recipes rm ID         # delete it
```

### MCP, file and web output

An MCP tool result with more than 20 KiB of text shows its first 20 KiB, cut at
a line end, and a notice with the full size. The complete text is written to a
private file in the session's `mcp-output` directory; the notice names that
path, and the model reads it with `read`. When the file cannot be written, the
notice says so. The instructions an MCP server sends are limited to 64 KiB.

`read` shares a 32 KiB budget across its files, stops at 2000 lines per file and
returns `next_offset` to continue. PNG and JPEG files arrive as images (64 MiB in
total per call). `web_fetch` renders at most 32 KiB by default (`max_output_kib`,
1 to 256) from a download of at most 8 MiB.

### Prompt cache and idle compaction

Providers bill a repeated request prefix at a lower price while their prompt
cache is warm. The `cache` segment of the interactive status line counts down
the time left, prefixed with `~` when ego estimates it from earlier cache
expiries. With `compaction.idle = true`, an idle interactive session compacts
shortly before that deadline, while reading the context is still cheap. The
`cache` and `compaction.*` keys are in the
[configuration reference](../reference/configuration.md#every-configtoml-key).

## Other model operations

| Operation | Purpose |
| --- | --- |
| `ask_question` | Suspend for a typed human answer; headless emits a question and exits 3 |
| `skill` | Invoke admitted named instructions or read their admitted resources |
| `goal` | Set a model-authored session goal when none is active; the model cannot replace any active goal, whoever set it. Automatic work on a model-set goal pauses after 20 continuation turns until the operator resumes it. See [goals](automation.md#goals) |
| `worklist` | Replace the durable ordered worklist or update one action status; this does not change or achieve the session goal |
| `context` | Request a compact/clear transition after the current tool batch; clear needs operator opt-in (`--context-clear`) and an active goal. Not offered with any `--tools` list or `--no-context-tool` |
| `task` | Run an isolated depth-one child with inherited tools, permission, sandbox, offline posture, and shared budget; the child cannot delegate |
| `history.search`, `history.open` | Read bounded prior user/assistant conversation through the store-bound history port; exclude tool payloads |
| `memory` | Off by default (`memory = true` in user configuration or `--memory`). Add, replace or remove one single-line `- ` entry of the workspace memory file (16 KiB limit); refused in plan mode. See [CLI.md §7.3](../../CLI.md) |
| `lsp` | Off by default (`lsp = true` in user configuration). Read-only `definition`, `references` and `diagnostics` queries through a language server that ego chooses from the file extension; the model cannot choose a command. See [CLI.md §7.4](../../CLI.md) |
| `build_recipe` | Off by default (`build_recipes = true` in user configuration). Save or inspect an exact-argv recipe that compacts the output of a repeated command. See [learned build recipes](#learned-build-recipes) |
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
or `worklist_step`. A foreground call can bind an existing `worklist` action with
`worklist_step`.

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

## Internal model catalogue

`model_catalogue` lists provider identities and pages known canonical model IDs.
Use optional `provider`, `offset` and `limit` (1–32; default 16). Keep the same
`snapshot` identity when following `next_offset`; restart the listing if it changes.
The result includes supported reasoning efforts, fact provenance and freshness,
and the current resolved model and effort. Unknown facts remain unknown. A
configured provider or a saved model list does not prove current reachability.

Listing reads existing supplied and cached facts. It does not request credentials,
run credential commands, log in, refresh tokens, make network requests, or write
configuration or caches. Use `ego models --refresh` for explicit live discovery.

### Per-task model and effort

Use optional `model` and `effort` on foreground or background `task` calls. Read
`model_catalogue` first to choose a canonical ID and a supported effort. Each
field uses this precedence: explicit call, named agent, `[task]` defaults, parent.
For `fork: true`, each field uses only the explicit call and the actual parent
profile; child defaults do not apply. The parent selection does not change.

A fork inherits the normalized conversation and effect ceilings. It does not
guarantee cache reuse across models or providers. The ordinary target adapter
must accept the inherited context and requested effort before generation. An
explicit model pin disables model fallback; unsupported requests fail visibly.
The existing `fork` restrictions with `background` and `agent` still apply.

Background tasks persist their admitted model and effort. Recovery and attachment
do not replace that selection with later parent or configuration values.
