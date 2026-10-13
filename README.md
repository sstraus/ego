# εgo

**Give any model a body. Keep the authority.**

εgo is a compact, provider-agnostic coding agent harness written in Rust.
Models reason and propose. εgo supplies tools, durable conversations, permissions,
confinement, and evidence, while keeping the model replaceable and authority in
the operator's hands.

> **The agent is not the model. The agent is the harness.**

```text
prompt → prepare → permit → execute → evidence
```

Use it when you want a coding agent in your terminal, a bounded agent step in a
script, or an agent behind an ACP host. The terminal, pipeline, and host share
the durable session machinery.

## Five-minute start

You need Rust 1.95 or newer and a model you can access. From a source checkout,
install into a directory you own (no published package or release is required):

```sh
cargo install --locked --path crates/ego-cli --root "$PWD/.ego-bin"
export PATH="$PWD/.ego-bin/bin:$PATH"
ego --help
```

The source includes a patched Reedline dependency in `vendor/`; keep the checkout
intact during installation. On this project's development Mac, Cargo must run
through the configured mbx shim; see [building from source](docs/getting-started/README.md).

Choose a provider/model from `ego models`. For example, with an existing local
Ollama installation:

```sh
ollama pull qwen3
ego models --provider ollama --refresh
ego config set 'roots=[]'
ego -m ollama/qwen3 --thinking off
```

Type a first prompt such as “Explain the main entry points in this repository.”
The model name above is an example: use one listed by your daemon.
`ego config set 'roots=[]'` selects workspace-only access instead of the default `~/Gits`
extra root, keeping unrelated repositories outside the perimeter. For a hosted
model, supply its API key or complete subscription login first; see
[providers and login](docs/guides/providers.md). No key belongs in project files.

| Entry point | Use it for |
| --- | --- |
| `ego` or `ego PROMPT` | Interactive conversation, model selection, permissions, and session controls |
| `ego run PROMPT` | One headless run; text, JSON, or live JSONL on stdout |
| `ego acp` | ACP over stdio for a host such as TUICommander |

The command set is `ego run`, `ego resume`, `ego continue`, `ego fork`, `ego compact`,
`ego export`, `ego ls`, `ego usage`, `ego models`, `ego hooks`, `ego doctor`,
`ego acp`, `ego a2a`, `ego auth`, `ego mcp`, `ego config`, `ego roots`,
`ego rules`, `ego orchestrator`, and `ego mcp-server`.

A pipeline uses an explicit model and never prompts for permission or trust:

```sh
printf '%s\n' 'Reply with one sentence about what a coding harness does.' | \
  ego run -m ollama/qwen3 --thinking off --no-tools --timeout 30s --output json -
```

Headless work is untrusted and bare by default. Project configuration and ambient
instructions do not silently become script authority. See
[permissions and confinement](docs/guides/permissions.md) before allowing effects.

## What it does

- Read, search, edit, write, and execute through prepared operations and permits.
- Keep durable sessions; resume, fork at a completed message, compact, and export.
- Select from local, hosted API, and subscription provider profiles through one
  model catalogue. Unsupported capabilities fail explicitly.
- Bound a run by tokens or money, turns, and time; retry only delivery-safe failures.
- Produce schema-validated answers and versioned structured output for scripts.
- Connect external MCP tools, serve scoped runs over MCP and ACP, and expose local A2A discovery.
- Load named skills and lifecycle hooks under the same authority boundaries.
- Inspect configuration, effective perimeter, usage, and redacted health facts.

Permission mode and operating-system confinement are separate choices. A relaxed
permission mode does not remove a sandbox or an administrator's ceilings.

## Status and platforms

This is an unreleased project; there is no stable release yet. `ego-cli` currently
has version `0.1.0` and workspace packages are not published. The command references
describe this checkout, not a promise about a future release.

macOS uses Seatbelt and Linux uses its confinement backends when the host can prove
the requested guarantee. Windows transport and private-state support exist, but
Windows confinement and the final cross-platform product proof remain open work.
An unavailable confinement guarantee is refused rather than silently weakened.
See [platforms and troubleshooting](docs/guides/troubleshooting.md) and
[the roadmap](ARCHITECTURE.mdregression case-resolves-48).

## Documentation

Start with the [documentation index](docs/README.md):

- [Getting started](docs/getting-started/README.md) and [interactive use](docs/guides/interactive.md)
- [Sessions, runs, turns, permits, and evidence](docs/concepts/README.md)
- [CLI reference](docs/reference/README.md) and [configuration reference](docs/reference/configuration.md)
- [Providers](docs/guides/providers.md), [tools](docs/guides/tools.md), and [permissions](docs/guides/permissions.md)
- [ACP, MCP, skills, hooks, and orchestration](docs/integrations/README.md)
- [Architecture](docs/architecture.md), [changelog](CHANGELOG.md), and [historical research](ARCHITECTURE.md)

## Name

**ε** is the smallest explicit bound. **ego** is the mediator between model intent,
policy, and the host. **go** moves language into action.

> **Minimum layer. Maximum agency. Explicit control.**

The [brand narrative](ARCHITECTURE.md) preserves the manifesto
and voice. The workspace declares the MIT license in `Cargo.toml`.
