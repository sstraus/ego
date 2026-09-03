# εgo

**Give any model a body. Keep the authority.**

εgo is a compact, provider-agnostic coding agent harness written in Rust.
Models can reason and propose. Real work also requires tools, memory,
boundaries, and evidence. εgo supplies that operational layer while keeping the
model replaceable and authority in the operator's hands.

> **The agent is not the model. The agent is the harness.**

```text
prompt → prepare → permit → execute → evidence
```

Scripting-first and Unix-native, εgo turns model output into governed action:
prompts enter, structured events leave, and the same durable session can move
between an interactive terminal, a pipeline, and an ACP host.

## Why εgo

- **Any model, one operational contract.** Provider differences stay at the
  edge instead of owning the workflow.
- **Actions earn authority.** Tools prepare canonical operations before policy
  permits them; execution cannot quietly widen what was approved.
- **Confinement is demonstrated.** A guarantee is exposed only when the current
  host and production path prove it.
- **Work leaves evidence.** Sessions, decisions, and tool results are durable,
  observable, and replayable.
- **Small enough for the Unix toolbox.** Use εgo interactively or compose it in
  scripts without adopting an IDE-sized platform.

## What works today

The working binary provides interactive sessions, headless runs, durable
resume/listing, session forks and compaction, local and structurally sanitized
exports, a shared current-model catalogue, subscription login, MCP configuration
management, atomic user-config management, read-only hook catalogue inspection,
an inbound MCP run server, and an ACP server. Ollama and OpenAI remain selectable directly; current
subscription profiles use `ego auth`.
Commands whose subsystem has not landed are refused by name instead of being
advertised as working.

## Quickstart

Available entry points are `ego [PROMPT]`, `ego run`, `ego resume`, `ego ls`,
`ego fork`, `ego compact`, `ego export`, `ego usage`, `ego models`, `ego mcp`,
`ego config`, `ego hooks`, `ego mcp-server`, and `ego acp`, plus `ego auth` for subscription
credentials. Promptless `ego resume [ID]` re-enters the interactive driver; a
resume carrying a prompt or `-` stays headless. `ego hooks ls` inspects the
admitted system and user hook catalogue without reading project configuration.

For a subscription profile, log in before selecting its model:

```sh
ego auth login openai-codex
ego -m openai-codex/gpt-5.3-codex
```

### Ollama

Start Ollama, install a model, then open an interactive εgo session:

```sh
ollama pull qwen3
ego models --provider ollama --refresh
cargo run -p ego-cli -- -m ollama/qwen3
```

εgo asks whether to trust project files before reading project config or
ambient instructions. Permission questions accept `allow`, `deny`, or `amend`;
an amendment is a complete JSON object containing the tool's replacement
parameters. `Ctrl+D` ends the session.

For a headless one-shot whose stdout can be piped:

```sh
cargo run -p ego-cli -- run -m ollama/qwen3 --output json \
  "Explain this repository" | jq .result
```

Headless runs never ask for trust or permission. They remain untrusted and
bare by default; use permission rules or a mode explicitly when a script needs
an effect. Run `cargo run -p ego-cli -- --help` for the commands available in
this checkout.

## Name

The name carries three ideas at once:

- **ε — the smallest explicit bound.** A compact layer with boundaries that
  are measured instead of assumed.
- **ego — the mediator.** The harness stands between model intent, policy, and
  the reality of the host.
- **go — from language to action.** εgo is built to do the work, not merely
  talk about it.

> **Minimum layer. Maximum agency. Explicit control.**

The complete positioning, manifesto, and voice live in
[`docs/22-brand-narrative.md`](docs/22-brand-narrative.md).

### Naming and distribution

Name availability was verified on 2026-08-19:

| Channel | State |
| --- | --- |
| binary `ego` in `PATH` | free |
| Homebrew formula | free |
| Homebrew cask | free |
| crates.io | taken — `intgr/ego` v1.2.0, "Alter Ego: run Linux desktop applications under a different local user", alive (updated 2026-01-08) |
| npm | taken |
| ego.sh / ego.dev | registered |

The crates.io collision blocks nothing: crate name and binary name are
independent (`ripgrep` ships `rg`). If we ever publish, it goes out as `ego-cli`
or `egoh` — both free — with `[[bin]] name = "ego"`. Distribution via GitHub
releases or a private Homebrew tap makes the question moot.

For comparison, `fx` is already a Homebrew formula (antonmedv's JSON viewer), so
the reference project has a worse collision than ours.

## Where to start

