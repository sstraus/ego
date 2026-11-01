# εgo

**Choose the model. Keep the workspace, history, and rules.**

εgo is a Rust coding-agent harness for your terminal, scripts, and ACP clients.
It gives local models, hosted APIs, and supported subscription providers the same
file and process tools, durable sessions, and permission controls. The model
proposes an action; the harness decides whether and where it can run.

## Control more than the prompt

### Separate permission from confinement

Review a repository without permitting edits, and select read-only operating-system
confinement independently:

```sh
ego --mode plan --sandbox ro "Explain the parser and identify missing tests"
```

Permission modes control which effects are allowed. Confinement controls the
filesystem boundary. Even `yolo` does not remove a sandbox. Inspect the admitted
boundary with `/config perimeter`, and inspect resolved settings with
`ego config ls --effective`.

macOS uses Seatbelt; Linux uses its supported confinement backends. If the host
cannot provide a requested guarantee, ego refuses rather than weakening it.
Windows supports execution and host protocols, but not `workspace`, `ro`, or
`--offline` confinement. See [permissions](docs/guides/permissions.md).

### Keep work beyond one conversation

Interactive and headless runs use the same durable session store. Reopen work,
branch a conversation, or export it without copying a terminal buffer:

```sh
ego ls
ego resume SESSION_ID
ego fork SESSION_ID
ego export SESSION_ID --format md
```

`ego compact` creates a compacted successor when context grows. Builtin file
writes and edits in Git worktrees also create session-owned checkpoints;
`ego checkpoint SESSION_ID preview` lists recovery points. Restore refuses
conflicting external changes instead of overwriting them.

### Give a task its own checkout

```sh
ego --worktree feat/parser-check "Review the parser and propose a focused fix"
```

The branch must be new. The checkout stays after exit, so unfinished work does
not disappear with the process. Use `ego worktree show RECEIPT_UUID` to inspect
it and `ego worktree cleanup RECEIPT_UUID` to request safe removal. Cleanup
refuses dirty or unmerged work; creating a worktree does not grant extra tool
authority. See [worktrees](docs/guides/worktrees.md).

### Change providers without changing the workflow

Use `ego models` to inspect the shared model catalogue, capabilities, and available
provider profiles. Select local Ollama models, hosted APIs, or supported
subscription logins with the same `-m provider/model` option. Unknown or
unsupported capabilities are not silently assumed.

With a running [Ollama](https://ollama.com) daemon:

```sh
ollama pull qwen3
ego models --provider ollama --refresh
ego -m ollama/qwen3 --thinking off "Explain the main entry points"
```

Use a model listed by your daemon. For hosted APIs and subscription login, see
[providers](docs/guides/providers.md); keep credentials out of project files.

### Use the same harness from a host or a pipeline

Configure an ACP client to launch `ego acp`. The host supplies the conversation
and permission UI; ego supplies the session and execution machinery. Connect
external tools through `ego mcp`, or expose scoped runs to an MCP client through
`ego mcp-server`. See [ACP and MCP](docs/integrations/README.md).

For scripts, select structured output and explicit run bounds:

```sh
printf '%s\n' 'Summarize what a coding harness does in one sentence.' | \
  ego run -m ollama/qwen3 --thinking off --no-tools \
    --max-turns 1 --timeout 30s --output json -
```

Headless runs never ask for trust or permissions and start bare and untrusted by
default. JSON and live JSONL output, schema-validated answers, token or money
budgets, turn limits, and timeouts let a caller define the run contract. See
[automation](docs/guides/automation.md) before enabling effects.

## Every command

Keep the whole workflow in one harness. See [CLI.md](CLI.md) for arguments and options.

- `ego run` — Give scripts a bounded, non-interactive agent run.
- `ego resume` — Reopen a durable session without losing its history.
- `ego continue` — Pick up an existing session with the resume alias.
- `ego fork` — Explore another direction without rewriting the original session.
- `ego rm` — Remove sessions you no longer need.
- `ego compact` — Carry the useful context into a compacted successor.
- `ego export` — Take session history into your own reports and tools.
- `ego ls` — Find the sessions attached to your work.
- `ego usage` — See where tokens and costs go.
- `ego models` — Find models and provider profiles for the next task.
- `ego hooks` — Inspect the hooks that extend your workflow.
- `ego doctor` — Diagnose your installation before starting work.
- `ego checkpoint` — Inspect recovery points and restore session-owned edits.
- `ego setup` — Configure saved settings through a staged screen.
- `ego acp` — Put the harness behind an ACP client's conversation UI.
- `ego a2a` — Expose local agent discovery over the A2A protocol.
- `ego auth` — Manage supported provider logins.
- `ego mcp` — Connect and manage external MCP tools.
- `ego config` — Inspect and change the settings that shape your runs.
- `ego roots` — Manage the filesystem roots available to the harness.
- `ego rules` — Inspect and manage tool permission rules.
- `ego mcp-server` — Expose scoped agent runs to an MCP client.
- `ego worktree` — Inspect retained checkouts and request safe cleanup.

## Install

On macOS and Linux:

```sh
brew install sstraus/tap/ego
```

Or use [release downloads](https://github.com/sstraus/ego/releases) and the
[installation and source-build guide](docs/getting-started/README.md).
Keep the complete checkout, including `vendor/`, when building from source.
Run `ego --version` to identify your installed build and `ego --help` for its
command set.

## Documentation

- [User guide](https://sstraus.github.io/ego/) · [guide source](docs/README.md)
- [Getting started](docs/getting-started/README.md) and [interactive use](docs/guides/interactive.md)
- [Sessions](docs/guides/sessions.md), [worktrees](docs/guides/worktrees.md), and [instructions and memory](docs/guides/memory.md)
- [CLI reference](docs/reference/README.md) and [configuration](docs/reference/configuration.md)
- [Permissions](docs/guides/permissions.md), [providers](docs/guides/providers.md), and [tools](docs/guides/tools.md)
- [ACP, MCP, skills, and hooks](docs/integrations/README.md)
- [Architecture](ARCHITECTURE.md), [contributing](CONTRIBUTING.md), and [release notes](docs/release-notes/0.4.3.md)

## License

εgo is licensed under [Apache-2.0](LICENSE). Releases through `v0.4.1` remain
under MIT. Vendored dependencies and other third-party material retain their own
licenses; see [third-party licenses](docs/THIRD_PARTY_LICENSES.md).
