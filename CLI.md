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
                              --name and --goal-operation are metadata-only operations
ego ls                        list sessions (this workspace; --all for every one)
ego rm <ID> [--yes] [--force]  tombstone one session; confirm unless --yes;
                              --force acknowledges an uncertain run
ego fork <ID> [--at ITEM_ID]   branch after a completed turn
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
                              run/continue/cancel/status tools, plus compact
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
ego acp [--tools LIST | --no-tools] [--allow RULE] [--deny RULE]
        [--profile NAME] [--append-system TEXT|@file] [--output-style NAME]
        [--instructions FILE]
        [--listen ENDPOINT [--allow-public-bind]
         [--max-connections N] [--connection-lifetime DURATION]
         [--max-ingress-bytes N]]
                              serve ACP on stdio by default, or on an explicit
                              endpoint. The seven session flags use the
                              general session shape and mean here exactly what
                              they mean for `ego run`; naming none of them is
                              the unshaped session. Listener controls default
                              to 32 connections, 24h, and 134217728 cumulative
                              ingress bytes per connection. TCP endpoints need
                              EGO_ACP_TOKEN; local endpoints use operating-
                              system peer identity and never read that token.
                              Public addresses additionally require
                              --allow-public-bind and cap `off` confinement at
                              `workspace`.
ego a2a --listen ENDPOINT [--allow-public-bind] [--sandbox LEVEL]
                              serve Agent2Agent discovery information.
                              ENDPOINT is a socket path —
                              authenticated by the operating system, the
                              preferred form — or an address, which requires a
                              credential in EGO_A2A_TOKEN. An address reachable
                              from outside this machine is refused by name
                              unless the operator asks for it
ego worktree show|cleanup RECEIPT_UUID   inspect or safely remove a retained local checkout
ego doctor [--json]           read-only redacted health snapshot
ego setup [--profile NAME] [--directory PATH]
                             terminal configuration guide; review and confirm before saving
```
<!-- ego-cli-command-set:end -->

| Command | Purpose |
| --- | --- |
| `ego run` | Run a prompt without interactive questions |
| `ego resume`, `ego continue` | Reopen durable work |
| `ego fork`, `ego compact` | Branch or reduce conversation context |
| `ego export`, `ego checkpoint` | Export a session or manage file recovery points |
| `ego ls`, `ego rm`, `ego usage` | Inspect or remove stored work and inspect usage |
| `ego models`, `ego auth` | Inspect models and manage provider login |
| `ego config`, `ego roots`, `ego rules` | Manage configuration and authority |
| `ego hooks`, `ego mcp` | Inspect hooks and configure MCP servers |
| `ego doctor`, `ego setup` | Diagnose the environment and configure saved settings |
| `ego acp`, `ego a2a`, `ego mcp-server` | Serve host protocols |
| `ego worktree` | Inspect or clean up retained worktrees |

A prompt can come from arguments, a file, or stdin. `-` selects stdin explicitly.
For agent-file input, see [automation](docs/guides/automation.md).

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
  "v": 18,
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
context overflow remains a separate typed failure.
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
| `budget` | yes | yes | yes |
| `bare` | yes | no | no |
| `permissions.judge` | yes | yes | yes |
| `ui.statusline.template` | yes | no | no |
| `ui.keybindings` | yes | no | no |
| `ui.vim` | yes | no | no |
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
