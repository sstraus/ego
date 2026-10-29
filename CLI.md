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
| `ego doctor`, `ego setup` | Diagnose the environment and review setup proposals |
| `ego acp`, `ego a2a`, `ego mcp-server` | Serve host protocols |
| `ego orchestrator`, `ego worktree` | Inspect orchestration or retained worktrees |

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

### Managed worktree launch

`ego --worktree feat/task "implement the task"` approves creation of a named Git
branch and retained checkout. Use the receipt with `ego worktree show` or
`ego worktree cleanup`. Cleanup refuses unsafe removal and leaves the branch.
See [worktrees](docs/guides/worktrees.md).

### 3.1 Availability — what ships today

The CLI does not silently relax a request that the selected model, protocol,
or platform cannot support.

## 4. Output contracts

### 4.1 `--output json` (one object on stdout at completion)

JSON output is a completion object, not a stream of terminal updates. Tool output
and diagnostics do not become extra JSON objects on stdout. See
[automation](docs/guides/automation.md) for parsing examples and schema validation.

### 4.2 `--output jsonl` (one event per line, live)

JSONL output emits structured events as work progresses. Consumers must parse
whole lines and use the documented versioned event fields. See the
[transcript reference](docs/reference/transcript.md).

### 4.3 Exit codes

Use process exit status to distinguish successful completion from refusal,
invalid input, budget exhaustion, cancellation, and runtime failure. The
[CLI overview](docs/reference/README.md) documents the exit-status table.

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

Use `/help` for slash commands, `/model` for model selection, `/mode` for permission
mode, and `/config perimeter` for the actual admitted boundary. Shift+Tab cycles
permission modes without submitting the draft. `/quit` exits without deleting
the durable session. See [interactive use](docs/guides/interactive.md).

Queued prompts, steering, questions, drafts, and cancellation are session controls;
they do not create a second policy path. Session compaction and forks preserve
explicit constraints through the shared lifecycle.

### Reviewed persistent memory and project initialization

`/init` reviews proposed workspace instructions before writing them. `/memory`
uses an optional external memory index and requires review for mutations.
Memory content is data, not system authority. See [memory](docs/guides/memory.md).

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
