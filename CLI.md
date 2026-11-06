# Command language

This document describes the public command conventions. For exact arguments, use
`ego --help`, `ego COMMAND --help`, or the
[generated command reference](docs/reference/cli.md).

## 1. Design principles

- Interactive use starts with `ego` or `ego PROMPT`.
- Scripts use `ego run` with an explicit model and output format.
- Diagnostics go to stderr. Requested structured output goes to stdout.
- Permission mode does not disable operating-system confinement.
- Session identity, tools, budgets, and policy are shared across terminal and host interfaces.

## 2. Command set

<!-- ego-cli-command-set:start -->
```
ego [PROMPT]                  interactive session (prompt submitted if given)
ego --continua                 directly resume latest saved session in this workspace
ego run [PROMPT]              headless one-shot; PROMPT from argv or stdin; -f FILE loads an agent file
ego continue [ID] [PROMPT]    alias of resume; same session selection and options
ego resume [ID] [PROMPT]      continue a session; no ID = interactive picker or headless latest;
                              --answer QUESTION_ID=JSON answers pending input;
                              --title TITLE selects the session by its exact title;
                              --name and --goal-operation are metadata-only operations
ego ls                        list sessions (this workspace; --all for every one)
ego rm <ID> [--yes] [--force]  tombstone one session; confirm unless --yes;
                              --force acknowledges an uncertain run
ego fork <ID> [--at ITEM_ID]   branch after a completed turn
ego rewind [-C DIR] <ID> --at ITEM_ID
                              restore the files changed after a turn, then fork there
ego checkpoint [-C DIR] <ID> preview|restore COMMIT|close
                              inspect or selectively undo builtin file edits
ego compact <ID> [--request-id UUID] [--budget LIMIT] [--max-output-tokens N]
                              publish one idempotent checkpoint target
ego export <ID> [--sanitize] [--format md|json|jsonl|transcript] [--file PATH]
                [--follow] [--after SEQ]
                [-C DIR] [--session-dir DIR]
                              render a session; Markdown is the default
ego mcp add NAME --stdio [--cwd DIR] [--inherit-env NAME]... -- COMMAND [ARG]...
        add NAME --http URL [--profile current|legacy-sse] [--auth none|oauth] [--allow-insecure-loopback]
        rm NAME | ls [--json] | login NAME | reload [NAME]
                              shared configuration and live admission service;
                              login NAME --client-id ID | --cimd URL | --dcr
ego mcp-server --max-concurrent-runs N --retained-requests N
               --max-resource-bytes N [-C DIR] [--session-dir DIR]
                              serve ego AS an MCP server on stdio: scoped
                              run/continue/cancel/status tools, plus M3 compact
                              (delegation surface for Claude, Codex, CI) plus
                              sanitized resources; no MCP Tasks or prompts
ego auth   login|logout|status [PROVIDER]
ego models [--provider P] [--refresh] [--json]
ego hooks  ls [--json]                     read-only admitted hook catalogue
ego config capabilities|snapshot|batch|get|set|unset|ls|edit
ego roots  add|rm|ls                         user perimeter roots
ego rules  ls|allow|deny|revoke|remember|explain   (permission rules,
                              stable IDs; `explain <tool> <args>` shows the
                              canonical intent + matching rule, dry)
ego usage  [--period P] [--all-roles] [--json] cost and token report
ego acp [--tools LIST | --no-tools] [--no-hooks] [--allow RULE] [--deny RULE]
        [--profile NAME] [--append-system TEXT|@file] [--output-style NAME]
        [--instructions FILE]
        [--listen ENDPOINT [--allow-public-bind]
         [--max-connections N] [--connection-lifetime DURATION]
         [--max-ingress-bytes N]]
                              serve ACP on stdio by default, or on an explicit
                              endpoint. The seven session flags are §3's
                              general session shape and mean here exactly what
                              they mean for `ego run`; naming none of them is
                              the unshaped session. Listener controls default
                              to 32 connections, 24h, and 134217728 cumulative
                              ingress bytes per connection. TCP endpoints need
                              EGO_ACP_TOKEN; local endpoints use operating-
                              system peer identity and never read that token.
                              Public addresses additionally require
                              --allow-public-bind. Authenticated LAN clients
                              use the selected sandbox ceiling; the address
                              does not force `adaptive` or `off` to `workspace`.
ego a2a --listen ENDPOINT [--allow-public-bind] [--sandbox LEVEL]
                              serve Agent2Agent discovery information.
                              ENDPOINT is a socket path —
                              authenticated by the operating system, the
                              preferred form — or an address, which requires a
                              credential in EGO_A2A_TOKEN. An address reachable
                              from outside this machine is refused by name
                              unless the operator asks for it
ego worktree show|cleanup RECEIPT_UUID   inspect or safely remove a retained local checkout
ego doctor [--json] [--offline] [--probe-network]  read-only redacted health snapshot
ego setup [--profile NAME] [--directory PATH]
                             terminal configuration guide for user defaults and profiles;
                             shows provider credential readiness (no secret read);
                             review and confirm before an atomic save
```
<!-- ego-cli-command-set:end -->

Put the subcommand before its flags: `ego acp --profile NAME`, not
`ego --profile NAME acp`. A command name parsed as an unescaped root prompt
after flags is refused with usage exit 2; it is never sent to the model. Use
`ego --plain -- acp` to submit the literal prompt `acp`. Command names used as
option values remain values, for example `ego --system acp "hello"`.

| Command | Purpose |
| --- | --- |
| `ego run` | Run a prompt without interactive questions |
| `ego resume`, `ego continue` | Reopen durable work |
| `ego fork`, `ego rewind`, `ego compact` | Branch, restore session-owned edits, or reduce conversation context |
| `ego export`, `ego checkpoint` | Export a session or manage file recovery points |
| `ego ls`, `ego rm`, `ego usage` | Inspect or remove stored work and inspect usage |
| `ego models`, `ego auth` | Inspect models and manage provider login |
| `ego config`, `ego roots`, `ego rules` | Manage configuration and authority |
| `ego hooks`, `ego mcp` | Inspect hooks and configure MCP servers |
| `ego doctor`, `ego setup` | Diagnose the environment, review provider readiness, and configure saved settings |
| `ego acp`, `ego a2a`, `ego mcp-server` | Serve host protocols |
| `ego worktree` | Inspect or clean up retained worktrees |

A prompt can come from arguments, a file, or stdin. `-` selects stdin explicitly.
For agent-file input, see [automation](docs/guides/automation.md).

When stdin of an interactive session is redirected, EOF preserves the last
turn's exit status, including provider or response-stream failure (4), invalid
provider input (2), and context overflow (4). A successful turn exits 0. On a
human terminal, closing the session after a provider or model refusal remains a
normal session close (0). Use `ego run -` for a one-shot stdin prompt with
headless retry defaults.

### Headless ACP stdio example

Configure an ACP client to launch `ego acp`. Its stdin and stdout carry protocol
messages, not terminal interaction. See [ACP setup](docs/integrations/acp.md).

### 2.1 Availability — what ships today

The generated reference describes this checkout. Installed builds can differ;
use `ego --version` and the installed executable's help when diagnosing a command.

### 2.2 Configuration and rule management grammar

`ego config ls --effective` shows resolved configuration. `ego roots` manages extra
roots; `ego rules` manages permission rules. Saved perimeter changes do not alter
an already admitted session. Restart or resume to admit the new configuration.
See [configuration](docs/reference/configuration.md).

## 3. Flags (the argument grammar)

Use `-m provider/model` to select a model. `--mode` selects `plan`, `default`,
`auto`, `accept-edits`, or `yolo`. `--sandbox` selects confinement separately.
`--offline` requests an enforced network restriction; unsupported guarantees are
refused. See [permissions](docs/guides/permissions.md).

`--output text|json|jsonl` selects output explicitly. `--no-tools` prevents tool
use. Token or monetary limits, turn limits, and `--timeout` bound work. Consult
the generated reference for units, defaults, and mutually exclusive options.

Headless runs default to three additional attempts for transient failures,
including provider capacity refusals (at most four requests per candidate).
Explicit `--retry N`, including zero, replaces that allowance; `--retry 0` makes
one attempt, so a script can own its retry policy. Interactive and ACP seats use
a longer recovery horizon.

`ego resume --accept-authority-delta` (also on `ego continue`) allows the
execution-authority change measured for that one resume without a question,
also headless. It cannot widen past administrator ceilings. Without it, a
headless resume of widened authority exits 3 and starts no provider call.

| Flags | Effect |
| --- | --- |
| `--allow` / `--deny` | Permission rules use the `read`, `write`, `list`, `search`, `exec`, `net`, and `tool` heads. Rules match prepared operations, not rendered shell strings. |

`-v` and `--verbose` are recognized but unavailable. Configure diagnostics with
`EGO_LOG` and `EGO_LOG_FILE` instead; see [troubleshooting](docs/guides/troubleshooting.md).

### Managed worktree launch

`ego --worktree feat/task "implement the task"` approves creation of a named Git
branch and retained checkout. Use the receipt with `ego worktree show` or
`ego worktree cleanup`. Cleanup refuses unsafe removal and leaves the branch.
See [worktrees](docs/guides/worktrees.md).

### 3.1 Availability — what ships today

The CLI does not silently relax a request that the selected model, protocol,
or platform cannot support.

## 4. Output contracts

<!-- ego-cli-json-output:start -->
### 4.1 `--output json` (one object on stdout at completion)

```json
{
  "v": 20,
  "session": "s_9f2k…",
  "result": "…final assistant text or --schema-validated object…",
  "exit_reason": "done|budget|max_turns|timeout|permission|question|paused|error",
  "turns": 4,
  "usage": { "uncached_input_tokens": 8110, "cache_read_tokens": 40100,
             "cache_write_tokens": 1200, "output_tokens": 3120 },
  "context": { "used_tokens": 51410, "provenance": "generation_boundary" },
  "cost": { "state": "known",
            "total": { "amount": "0.083", "currency": "USD" } }
}
```

`v` is **`PUBLIC_EVENT_VERSION`**, the same number §4.2's events carry, and not
a number of its own. This object is a projection of the same public model —
`usage`, `cost` and `context` are that vocabulary's types, so a change to them
changes both surfaces at once, and a second number would let one of the two
move without the other. §4.6's catalogue is versioned separately for the
opposite reason: it publishes no part of the event model.

When the session has a goal, the summary also contains `goal`:
`{"status":"achieved|stopped|exhausted|active","reason":"…"}`.
The field is absent for sessions without a goal. `achieved` means the durable
goal was achieved; it has no `reason`. `stopped` carries the goal-check stop
reason (including bounded evaluator failure diagnostics), or the blocker reason.
`exhausted` carries the goal continuation or goal-check token budget reason.
`active` means the goal remains open without a goal-check stop, for example
when the invocation needs human input. Its last blocker reason is included if
present. `ok` and `exit_reason` still describe the invocation, not goal success:
a completed invocation can have `ok:true`, `exit_reason:"done"`, and a stopped
or exhausted goal. Consumers that require goal success must inspect `goal.status`.

`usage` is the sum of the per-generation deltas, split by the categories that
are *priced differently*; it is a flow and only grows. `context` is the
occupancy *level* of the live context window and goes **down** after a
compaction, so it is reported separately and never derived from `usage`.
The last measured occupancy is not clamped to the model window. A delivered
final answer completes its turn even if usage reaches or exceeds the resolved
window. Before the next provider request, a known-full context is refused with
`context_overflow`, asking for `/compact` or a larger model window. Unknown occupancy and history added since
the measurement are not an exact preflight token count; a provider-reported
context overflow remains a separate typed failure. With `compaction.headless = true`
(off by default), the run compacts and continues in the successor, subject to
the same budget, deadline and turn limit. Otherwise it exits 4. A failed
compaction also exits 4.
`cost.amount` is an exact decimal **string**, not a JSON number — binary floats
cannot represent a price. `cost.state` is `unstarted` before the first priced
generation, and `{"state":"unknown","reason":…}` once any generation could not
be priced: an unpriceable generation makes the session total unknown rather
than silently under-reported.

On exit 3, exactly one human-input payload is present. A permission boundary
uses the existing `denied`/`denied_by` fields. An `AskQuestion` boundary uses a
`question` object containing its stable ID, prompt, choice mode, options, and
whether free text is accepted. A script answers it with
`ego resume ID --answer QUESTION_ID=JSON`; a mismatched ID or value is exit 2,
not an answer guessed against a different question.

`paused` is the one reason that carries **no** payload and asks nothing. The
session stopped because a host called `_ego/pause`, and
the only thing that moves it again is that host calling `_ego/resume`. It is
exit 6 and deliberately not exit 3 for exactly that reason: a script that reads
3 goes looking for a `denied`/`denied_by` pair or a `question` object, and a
pause has neither.

<!-- ego-cli-json-output:end -->

### 4.2 `--output jsonl` (one event per line, live)

JSONL output emits structured events as work progresses. Consumers must parse
whole lines and use the documented versioned event fields. See the
[transcript reference](docs/reference/transcript.md).

### 4.3 Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Completed |
| 1 | Task failure or internal runtime failure |
| 2 | Invalid input or usage |
| 3 | Permission or question requires human input |
| 4 | Provider, network, uncertain delivery, or context-overflow failure |
| 5 | Budget or maximum-turn limit |
| 6 | Host-requested safe-boundary pause |
| 7 | Local generation guard |
| 124 | Timeout |
| 130 | SIGINT |
| 143 | SIGTERM |

Completion reasons and exit codes are different classifications. For example,
`budget` and `max_turns` share code 5. A pause has no human-input payload;
permission and question outcomes do. See `RunTermination` in
`crates/ego-core/src/run.rs` for the shared mapping.

### 4.4 Non-interactive posture: untrusted by default

Headless runs do not ask for workspace trust or tool permission. They start bare
and untrusted by default. Supply explicit instructions and deliberate policy
for automation that needs effects. A prompt does not grant authority by itself.

### 4.5 Parse and lifecycle contract

Invalid arguments fail before a run starts. A served protocol owns its transport;
normal CLI text must not enter a protocol stdout stream. Durable completion and
cancellation are recorded by the shared run lifecycle.

### 4.6 `ego ls --output json|jsonl` (the catalogue)

Session listing uses stored metadata and reports unreadable entries without
silently presenting them as healthy sessions. See [sessions](docs/guides/sessions.md).

### 4.7 `--output text` (the default): two renderings, one gate

Interactive rendering can use terminal capabilities. Pipes receive plain text
without interactive controls. Permission and budget decisions are not changed
by rendering mode.

### 4.8 Diagnostics (`EGO_LOG`, `EGO_LOG_FILE`)

Diagnostic logging is separate from stdout output contracts. Do not put secrets
in diagnostics or shared transcripts. See [troubleshooting](docs/guides/troubleshooting.md).

## 5. Agent files — the scripting language

Agent files combine reviewed configuration, instructions, and a prompt template.
They are executable inputs, not automatically trusted project data. See
[automation and agent files](docs/guides/automation.md).

### 5.1 Skills are in-session capabilities, not agent files

Skills are progressively loaded instructions and resources. Their use remains
subject to the active session authority. See [skills](docs/integrations/skills.md).

## 6. Interactive surface

Interactive startup shows the selected three-row εgo braille logo directly above
the application name and version, before configuration,
trust, provider, or MCP notices. It reports the selected model, effective posture,
and key hints after admission. Startup notices remain visible; `--quiet` keeps
its existing announcement policy. The logo requires interactive stdin and TTY stdout
and at least 17 observed terminal columns. It keeps the same glyphs without SGR
when colour is disabled (including `NO_COLOR`); narrow terminals keep the identity
line only. Headless and protocol modes do not show the logo. The name does not
require a resolved model.

Use `/help` for slash commands, `/model` for model selection, `/mode` for permission
mode, and `/config perimeter` for the actual admitted boundary. Shift+Tab cycles
permission modes without submitting the draft. `/quit` exits without deleting
the durable session. See [interactive use](docs/guides/interactive.md).

Queued prompts, steering, questions, drafts, and cancellation are session controls;
they do not create a second policy path. Session compaction and forks preserve
explicit constraints through the shared lifecycle.

### Project instructions and external memory

`/init` reviews proposed workspace instructions before writing them.
External memory integrations use configured hooks. See [memory](docs/guides/memory.md).

## 7. Configuration

Configuration layers supply defaults and ceilings. Project configuration and
instructions require workspace trust. Inspect provenance with
`ego config ls --effective`; see [configuration](docs/reference/configuration.md).

### 7.1 Operator-supplied identity headers

Default provider requests use ego's identity where a client identity is declared.
Some services can reject clients that do not use their own application identity.
Review provider terms before configuring `[auth.<provider>.headers]` overrides.
Authorization credentials and protocol-critical headers cannot be supplied as
arbitrary identity overrides. See [providers](docs/guides/providers.md).

### 7.2 `models.toml` — hosted model capabilities, in three layers

Builtin, system, and user model definitions merge per model slug. Stronger layers
override only the models they name. Unknown capabilities and prices remain
unknown. First-run defaults and service-tier settings are separate from
capability declarations. See [providers](docs/guides/providers.md) and
[configuration](docs/reference/configuration.md).

When no model is configured, interactive startup selects and saves the default
of the single usable provider, or opens the model picker. A headless run with
exactly one usable provider uses that default for the run, names it on stderr,
and does not save it; otherwise it refuses and names the remedies.

A profile can set `cache_ttl_seconds` as a display-only estimate of a
provider-managed prompt-cache lifetime. It drives the interactive cache
countdown and sends no provider parameter. Opt-in idle compaction
(`compaction.idle`, off by default) compacts an idle interactive session shortly
before that countdown expires. See [configuration](docs/reference/configuration.md).

### 7.3 Managed configuration write scopes

The matrix lists keys accepted by managed configuration writes. `yes` means
the key is writable in that target; `no` means the write is refused. An
ordinary profile and the reserved `ci` profile have distinct scopes.

<!-- ego-managed-config-keys:start -->
| Key | User base | User profile | ci profile |
|---|---|---|---|
| `model` | yes | yes | yes |
| `thinking` | yes | yes | yes |
| `mode` | yes | yes | yes |
| `sandbox` | yes | yes | yes |
| `roots` | yes | yes | yes |
| `exec_paths` | yes | yes | yes |
| `exec.env` | yes | yes | yes |
| `exec.env_allowlist` | yes | yes | yes |
| `network` | yes | yes | yes |
| `web_private_network` | yes | no | no |
| `budget` | yes | yes | yes |
| `bare` | yes | no | no |
| `permissions.judge` | yes | yes | yes |
| `ui.statusline.template` | yes | no | no |
| `ui.keybindings` | yes | no | no |
| `ui.vim` | yes | no | no |
| `compaction.idle` | yes | no | no |
| `compaction.idle_margin_seconds` | yes | no | no |
| `compaction.idle_threshold` | yes | no | no |
| `compaction.headless` | yes | no | no |
| `doctor_network` | yes | no | no |
| `recap.enabled` | yes | no | no |
| `recap.after_minutes` | yes | no | no |
| `mcp_tasks` | yes | no | no |
| `memory` | yes | no | no |
| `checkpoints` | yes | no | no |
| `tools` | yes | no | no |
| `sessions.compress` | yes | no | no |
| `cache` | yes | no | no |
| `instructions.max_files` | yes | no | no |
| `lsp` | yes | no | no |
| `session_dir` | no | no | yes |
| `artifact_dir` | no | no | yes |
| `auth.PROVIDER.headers` | yes | no | no |
| `auth.PROVIDER.credential_command` | yes | no | no |
<!-- ego-managed-config-keys:end -->

## Current interactive controls

`ego --continua` resumes the most recently updated readable session in the current workspace without a picker.

`/goal TEXT` and `/goal set TEXT` save an operator-owned objective and submit its first work turn. `/goal extend N` grants more automatic turns to the active goal.

A human terminal can grant session roots during permission admission. `/config roots revoke EXACT_PATH` revokes an exact admitted path and rebuilds the same session under narrower authority. See the [configuration reference](docs/reference/configuration.md#revoke-a-terminal-root-grant).

`/setup` configures saved settings through a staged screen; `/doctor` reports diagnostics. Interactive shell escapes start with `!`; permission mode and OS confinement still apply.

Configured provider protocol filters transform admitted request and response bytes. See the [protocol-filter guide](docs/integrations/protocol-filter.md). ACP hosts can negotiate child activity independently; see [ACP extensions](docs/integrations/acp-extensions.md#child-activity-negotiated-version-1).

Submitted prompts are echoed in full, including prompts taller than the terminal.
`/expand` shows the full output of the last tool call.

## Session and tool contracts

`ego resume --title TITLE` and `ego continue --title TITLE` select an exact,
case-sensitive title in the current workspace. No match or a title combined
with an id is a usage error. Duplicate titles list matching sessions and
resume none. Every positional with `--title` is prompt text.

`--no-session-compression` skips background compression of old sessions for
this run; `sessions.compress = false` disables it in configuration. The `tools`
configuration list restricts builtin tools, and `--tools LIST` overrides it.

Doctor makes no network call by default. `ego doctor --probe-network` or
`doctor_network = true` in user configuration enables unauthenticated DNS,
TCP, TLS and HTTP reachability probes. `/doctor` uses the configuration opt-in.
Offline policy always skips the probes. Probes never send credentials or make
a paid provider request.

### Queued resume

`ego resume ID --queue PROMPT` (also `-f FILE` or `-` for stdin) queues a
  prompt for a saved session that another process may own. It waits while an
  interactive session, another resume, an ACP connection or an earlier queued
  prompt holds the session, then runs the prompt headless through the same
  parse, configuration, trust, permission, sandbox and budget admission as
  `ego resume ID PROMPT`. Queued prompts on one session run one turn at a
  time; their order is not guaranteed. The exit status is that turn's status
  (§4.3). The wait counts toward `--timeout` (exit 124) and ends on
  SIGINT/SIGTERM (130/143). `--queue` requires a prompt and is refused with
  `--answer` and with profile `ci`. Without an ID it pins the latest session
  of the workspace before it waits.

### Rewind

`ego rewind ID --at ITEM_ID` returns the conversation and the files to the same
turn. It selects the turn as `ego fork --at` does, takes the session's lease
(so the session must not be running), and finds the file checkpoints that the
session's builtin `write`, `edit` and `notebook_edit` results recorded after
that turn (see "Session-owned file checkpoints"). Before it changes anything,
it checks each file against the disk and against every later write of it, and
refuses, listing the files, when a file changed outside ego after that turn,
also between two of those writes; keep or undo those changes first. It also refuses when the checkpoint chain holds writes after the turn
that no result records (a `task` child's writes, or writes from a version that
did not record checkpoint ids), when a write after the turn saved no checkpoint
(checkpoints are off, or the file is outside Git), and when a recorded checkpoint
is gone from the repository. Otherwise it restores the checkpoints newest first and then creates
the fork. Text output is the new session ID, with one `restored PATH` note per
changed file; `--output json` lists every checkpoint with its path and effect.
The source session is not changed: its journal stays, each restore is a restore
checkpoint in its ref, and the new session's lineage names the source and the
turn. Exec commands and MCP effects are not captured, so they are not undone.
Use `-C` for the repository the session wrote to. A failure while restoring
leaves the earlier restores applied and creates no session; the same rewind can
be run again, because a completed restore repeats safely.

### Bounded reads

The `read` tool accepts `{"files":[{"path":"src/main.rs","offset":0}]}`,
with one to sixteen entries, including for a single-file read. It resolves and
admits every path before reading any file; duplicate resolved targets are refused.
Results are ordered JSON items with their index, requested path and status. A
successful item includes content and `next_offset`; an execution failure includes
an error and does not omit later items.

The complete serialized result, including metadata and JSON escaping, is bounded
to 32 KiB, with at most 2000 content lines per file. A truncated item names its
next original-byte offset. Continue with that item's path and offset, including
inside a long line; a later item can receive no content when an earlier item uses
the shared budget, so continue that item separately. UTF-8 characters are not
split at the page boundary. Even small files use the structured result. The
guarded filesystem read and its permission checks remain in effect; this is a
provider-visible output bound, not a filesystem memory bound.

### Language-server code intelligence

The `lsp` tool is off. Turn it on with the `lsp` row in `ego setup` or
`ego config set lsp=true`. It starts processes, so only the user configuration
can set it; a project or local file that sets `lsp` is refused.

When on, the model can call `lsp` with `operation` `definition` or `references`
and the 1-based `line` and `column` (in characters) of a symbol, or with
`diagnostics` and a file. The file must be inside the workspace. The answer
lists at most 50 entries and 16 KiB: `path:line:column` and the source line (cut
at 200 characters; source text only for files inside the workspace), or the
line, column, severity, source, code and message of each diagnostic, then a
count of the omitted entries. Diagnostics wait up to 20 seconds for the server to
publish them; when it does not, the answer says so and does not claim the file
is clean. A server that is still indexing can answer nothing.

ego chooses the server from the file extension and starts it at the first query
for that language: `rust-analyzer` (`.rs`), `gopls` (`.go`), `clangd`
(`.c`, `.h`, `.cc`, `.cpp`, `.hpp`), `pyright-langserver --stdio` (`.py`) and
`typescript-language-server --stdio` (`.ts`, `.tsx`, `.js`, `.jsx`). The model
cannot choose a command. Install the server on `PATH`; a missing server is a
tool error that names it. A sandbox that refuses the server's executable, as it
refuses any command outside the exec perimeter, is a tool error that names the
path to add to `exec_paths`. The server starts in the workspace with the `exec`
environment, through the same guarded process launch and sandbox as an MCP
stdio server, so it has the run's confinement, and the offline posture refuses
it. Its standard error is kept as session evidence. One server per command
runs while ego runs and is reused by later queries; a server that exited starts
again at the next query. ego passes its process ID, and the server's standard
input closes when ego exits. ego sends the file text
from disk before each query. The tool only reads, so plan mode admits it, and
it asks no permission question: turning it on is the authority.

### MCP Tasks

MCP Tasks are off. Turn them on with the `mcp_tasks` row in `ego setup` or
`ego config set mcp_tasks=true`; only the user configuration sets it. A project or
local file that sets `mcp_tasks` is refused.

With the option on, an interactive human session declares the
`io.modelcontextprotocol/tasks` extension to HTTP MCP servers. A server may then
answer a `tools/call` with a task instead of a result. Ego answers the model's call
at once with the task id, the server and the tool, and the turn goes on. Ego asks the
server `tasks/get` for each live task when a generation starts, no more often than
every 500 ms and no less often than every 30 s (the server's suggested interval
sets the pace), and gives the model each result once, as context, when the task
completes, fails, is cancelled, expires or cannot be polled. While the session
waits at its prompt, ego also polls each live task when it is due; when one ends,
one generation starts on its own and carries the result, and a poll that finds the
task still working starts none.

Each poll runs under the grant that let the original call run, so a call you
approved is not asked about again at every poll. Ego polls and cancels only task
ids that its own call recorded; the model cannot name one. A task that needs input
from the client (`input_required`) is cancelled and the model is told.

Tasks end with the session: when it ends (exit, `/new`, resume, fork or compaction)
ego asks each server to cancel every live task of that session, once, and `ego resume`
does not resume them. The model's receipt says so, so after a resume it treats the task as lost. Stdio and ACP servers never get the extension (each call
there starts its own server, so the task would end before the first poll), and
headless `ego run` and `ego acp` do not use it.

### Checkpoint configuration

Checkpoints are on by default. Turn them off with the `checkpoints` row in `ego setup`,
`ego config set checkpoints=false`, or `--no-checkpoints` for one run. Only the user
configuration and the command line set the key; a project or local file that sets it is
refused. With checkpoints off, `write` and `edit` create no Git object and no ref, and
each result says that checkpoints are off, so nothing can be restored for those edits.

### Exact text edits

```json
{"files":[{"path":"src/example.txt","chunks":[{"old_string":"before","new_string":"after"}]}]}
```

Each chunk matches the original file and its span must not overlap another
chunk. By default, each old string must occur exactly once. `replace_all=true`
requires one chunk and replaces all non-overlapping occurrences of its nonempty
old string. All reads and writes need admission before execution. Execution
stops on the first failure; earlier file changes are not rolled back.

