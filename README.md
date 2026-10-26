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

## Install

Published installation methods below become available after the first GitHub
release and Homebrew tap setup. This checkout does not yet have a Git remote;
these are publication instructions, not evidence that a release exists.

### Homebrew (macOS and Linux)

```sh
brew install sstraus/tap/ego
```

### Direct download

The release provides plain executables and individual SHA256 files:

| Platform | Asset |
| --- | --- |
| macOS Apple Silicon | `ego-macos-arm64` |
| macOS Intel | `ego-macos-x64` |
| Linux x86_64 | `ego-linux-x64` |
| Linux ARM64 | `ego-linux-arm64` |
| Windows x86_64 | `ego-windows-x64.exe` |

Linux binaries require GLIBC 2.35 or newer. Target-triple `.tar.gz` archives
(`.zip` on Windows) remain available, together with `SHA256SUMS`.
For example, on Apple Silicon, download and verify before installation:

```sh
asset=ego-macos-arm64
base=https://github.com/sstraus/ego/releases/latest/download
curl -fLO "$base/$asset"
curl -fLO "$base/$asset.sha256"
shasum -a 256 -c "$asset.sha256"
mkdir -p "$HOME/.local/bin"
install -m 755 "$asset" "$HOME/.local/bin/ego"
```

On Linux use the matching asset and `sha256sum -c`. On Windows download
`ego-windows-x64.exe` and its `.sha256`, compare with PowerShell
`Get-FileHash -Algorithm SHA256`, then rename it to `ego.exe` in a directory on
`PATH`. Add `~/.local/bin` to `PATH` on macOS/Linux if needed.

### From source

You need Rust 1.95 or newer, Python 3 for the local release helper, and a model
you can access. From a source checkout, build and atomically install with
`scripts/local-release.sh`; it installs to `~/.local/bin/ego` and does not stop
existing sessions. On the development Mac, invoke it with the configured mbx
shim on `PATH` and the build-slot wrapper. Do not override `CARGO_TARGET_DIR`.

Alternatively, install into a checkout-local directory (no published package or
release is required):

```sh
cargo install --locked --path crates/ego-cli --root "$PWD/.ego-bin"
export PATH="$PWD/.ego-bin/bin:$PATH"
ego --help
```

The source includes a patched Reedline dependency in `vendor/`; keep the checkout
intact during installation. On this project's development Mac, Cargo must run
through the configured mbx shim; see [building from source](docs/getting-started/README.md).

## Five-minute start

Choose a provider/model from `ego models`. For example, with an existing local
Ollama installation:

```sh
ollama pull qwen3
ego models --provider ollama --refresh
ego config set 'roots=[]'
ego -m ollama/qwen3 --thinking off --sandbox workspace
```

Type a first prompt such as “Explain the main entry points in this repository.”
The model name above is an example: use one listed by your daemon.
`ego config set 'roots=[]'` clears configured extra roots, including the implicit
`~/Gits` extra root under strict confinement. The example also explicitly selects
`workspace` confinement; macOS's default adaptive sandbox can derive additional
roots. Inspect `/config perimeter` for the actual admitted boundary. For a hosted
model, supply its API key or complete subscription login first; see
[providers and login](docs/guides/providers.md). No key belongs in project files.

| Entry point | Use it for |
| --- | --- |
| `ego` or `ego PROMPT` | Interactive conversation, model selection, permissions, and session controls |
| `ego run PROMPT` | One headless run; text, JSON, or live JSONL on stdout |
| `ego acp` | ACP over stdio for a host such as TUICommander |

The command set is `ego run`, `ego resume`, `ego continue`, `ego fork`, `ego compact`,
`ego export`, `ego checkpoint`, `ego ls`, `ego rm`, `ego usage`, `ego models`, `ego hooks`, `ego doctor`, `ego setup`,
`ego acp`, `ego a2a`, `ego auth`, `ego mcp`, `ego config`, `ego roots`,
`ego rules`, `ego orchestrator`, `ego worktree`, and `ego mcp-server`.

`ego checkpoint SESSION_ID preview` lists session-owned file checkpoints.
Use `ego checkpoint SESSION_ID restore CHECKPOINT_COMMIT` to restore one
checkpoint, or `ego checkpoint SESSION_ID close` to release its recovery ref
without changing files. Builtin `write` and `edit` capture checkpoints in Git
worktrees; external changes cause restore to refuse rather than overwrite them.

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

The public permission modes are `plan`, `default`, `auto`, `accept-edits`, and
`yolo`. Select them with `--mode NAME` or live `/mode NAME`; Shift+Tab cycles
the same modes without submitting the draft. Mode selection comes from
[the shared mode definition](crates/ego-core/src/config/layer.rs); terminal
routing is in [REPL actions](crates/ego-cli/src/repl/actions.rs).

Codex subscription generation uses WebSocket continuation with guarded SSE
fallback; see [Codex transport](docs/guides/providers.md#codex-transport).
Remote MCP OAuth login and runtime attachment are code-landed, but the real
operator login/restart check remains pending; see [MCP login](docs/integrations/mcp.md#remote-oauth-login).

Permission mode and operating-system confinement are separate choices. A relaxed
permission mode does not remove a sandbox or an administrator's ceilings.

## Status and platforms

Release `0.3.0` is being prepared locally; it is not published. See the
[release notes](docs/release-notes/0.3.0.md) for changes since `0.2.0`. The version is
defined once in
`[workspace.package]` in `Cargo.toml`; all workspace packages inherit it and are
not published. This release preparation selects `0.3.0`; normal landings increment
the patch version by one (`0.0.1`). Run `ego --version` to identify an installed
build. The command references describe this checkout, not a promise about a future release.

Recorded SSE comments, raw PTY captures, and dated validation reports retain
the version of the observed build. They are historical evidence, not version
declarations. SSE decoders ignore provenance comments; launch golden comparisons
normalize the banner version. Do not rewrite these records for a version bump.

macOS uses Seatbelt and Linux uses its confinement backends when the host can prove
the requested guarantee. Windows transport and private-state support exist, but
Windows `workspace`, `ro`, and `--offline` confinement remain unsupported.
The final product proof remains open.
An unavailable confinement guarantee is refused rather than silently weakened.
See [platforms and troubleshooting](docs/guides/troubleshooting.md) and
[the roadmap](ARCHITECTURE.mdregression case-resolves-48).

## Documentation

Start with the [user guide](docs/README.md). It is organized as an mdBook with
getting-started, task guides, integrations, and reference sections. With mdBook
0.5.4 and Python 3 available, build it with `mdbook build docs` or preview it
with `mdbook serve docs`; see [documentation maintenance](docs/maintaining.md).
These commands build or serve locally; they do not publish a website.

- [Getting started](docs/getting-started/README.md) and [interactive use](docs/guides/interactive.md)
- [Git worktrees](docs/guides/worktrees.md) and [instructions and memory](docs/guides/memory.md)
- [Sessions, runs, turns, permits, and evidence](docs/concepts/README.md)
- [CLI reference](docs/reference/README.md) and [configuration reference](docs/reference/configuration.md)
- [Providers](docs/guides/providers.md), [tools](docs/guides/tools.md), and [permissions](docs/guides/permissions.md)
- [ACP, MCP, skills, hooks, and orchestration](docs/integrations/README.md)
- [Architecture](docs/architecture.md) and [open-source licenses](docs/THIRD_PARTY_LICENSES.md)

## Name

**ε** is the smallest explicit bound. **ego** is the mediator between model intent,
policy, and the host. **go** moves language into action.

> **Minimum layer. Maximum agency. Explicit control.**

The [brand narrative](ARCHITECTURE.md) preserves the manifesto
and voice. The workspace declares the MIT license in `Cargo.toml`.

### Retained worktrees

Use `ego --worktree feat/task "implement the task"` or
`ego run --worktree feat/task "implement the task"` to approve a new branch and
launch from an isolated Git checkout. Worktrees remain after exit. Inspect the
printed receipt with `ego worktree show RECEIPT_UUID`; request safe cleanup with
`ego worktree cleanup RECEIPT_UUID`. Cleanup refuses local changes, ignored files,
unmerged commits and new commits without upstream evidence. The branch remains.
TUIC-hosted launches use TUIC ownership and lifecycle controls instead.
