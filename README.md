# εgo

**Choose the model. Keep the workspace, history, and rules.**

εgo is a coding agent for your terminal, your scripts, and ACP-capable editors and
hosts, written in Rust. It gives local models, hosted APIs, and supported
subscription logins the same file and process tools, durable sessions, and
permission controls. The model proposes an action; εgo decides whether, and where,
it may run.

Website: <https://ego.tuicommander.com> · User guide:
<https://ego.tuicommander.com/docs/>

## Why εgo

- **Every action is checked before it runs.** Permission modes decide what is
  allowed; operating-system confinement decides what the process can reach. When
  the host cannot provide a requested guarantee, εgo refuses instead of quietly
  weakening it.
- **Token economy built in.** εgo keeps the prompt prefix cacheable, shows how long
  the cache stays warm, folds noisy build output, pages large output on demand, and
  lets subagents start from the parent's cached context. Compacting an idle session
  before its cache expires is available as an option.
- **Sessions you can audit, branch, and undo.** Every session is a durable log. Fork
  at a completed turn, rewind files and conversation together, and restore Git
  checkpoints that refuse to overwrite edits made outside εgo.
- **A contract for scripts and CI.** Headless runs start untrusted, return
  schema-validated JSON, stop at token, cost, turn, or time limits, retry only when
  that is safe, and exit with codes that say why. Agent files and goals turn a
  prompt into a repeatable job.
- **One binary for every host and any model.** The same session machinery serves the
  terminal, scripts, ACP editors, and MCP clients, across local, hosted, and
  subscription providers.

## Install

Releases are published on
[GitHub Releases](https://github.com/sstraus/ego/releases). `ego --version` prints
the version you have; the [changelog](https://ego.tuicommander.com/docs/changelog.html)
lists what each release changed.

### Homebrew (macOS and Linux)

```sh
brew install sstraus/tap/ego
```

Homebrew installs Bash, Zsh, and Fish completions. PowerShell and Elvish scripts are
in `$(brew --prefix ego)/share/ego/completions/`.

### Direct download

Each release provides a plain executable and a `.sha256` file per platform:

| Platform | Asset |
| --- | --- |
| macOS Apple Silicon | `ego-macos-arm64` |
| macOS Intel | `ego-macos-x64` |
| Linux x86_64 | `ego-linux-x64` |
| Linux ARM64 | `ego-linux-arm64` |
| Windows x86_64 | `ego-windows-x64.exe` |

Linux binaries need glibc 2.35 or newer. Each release also has target-triple
archives (`.tar.gz`, or `.zip` on Windows) that include shell completions, and a
combined `SHA256SUMS` file. On Apple Silicon, for example:

```sh
asset=ego-macos-arm64
base=https://github.com/sstraus/ego/releases/latest/download
curl -fLO "$base/$asset"
curl -fLO "$base/$asset.sha256"
shasum -a 256 -c "$asset.sha256"
mkdir -p "$HOME/.local/bin"
install -m 755 "$asset" "$HOME/.local/bin/ego"
```

On Linux, use the matching asset and `sha256sum -c`. On Windows, compare the output
of `Get-FileHash -Algorithm SHA256` with the `.sha256` file, rename the download to
`ego.exe`, and place it in a directory on `PATH`.

### From source

You need Rust 1.95 or newer and the complete checkout (patched dependencies live in
`vendor/`):

```sh
cargo install --locked --path crates/ego-cli
```

Installing writes only the executable. εgo creates `~/.ego` (or `$EGO_HOME`) the
first time it saves a setting, a login, or a session.

## First run

1. Make one provider usable: export its API key (for example `ANTHROPIC_API_KEY` or
   `OPENAI_API_KEY`), log in to a supported subscription with
   `ego auth login PROVIDER`, or start a local [Ollama](https://ollama.com) daemon.
   εgo does not read `.env` files; keep keys out of project files.
2. Run `ego setup` to see which providers are ready, or run `ego` in a repository.
   With exactly one usable provider, εgo selects its default model; otherwise it
   opens the model picker.

With Ollama (use a model your daemon lists):

```sh
ollama pull qwen3
ego models --provider ollama --refresh
ego -m ollama/qwen3 --thinking off "Explain the main entry points in this repository"
```

## Three ways to run it

| Entry point | Use it for |
| --- | --- |
| `ego` or `ego PROMPT` | Interactive conversation with permission prompts and session controls |
| `ego run PROMPT` | One headless run with text, JSON, or live JSONL output |
| `ego acp` | Agent Client Protocol over stdio, for an editor or a host such as TUICommander |

A bounded script step:

```sh
printf '%s\n' 'Summarize what a coding agent harness does in one sentence.' | \
  ego run -m ollama/qwen3 --thinking off --no-tools \
    --max-turns 1 --timeout 30s --output json -
```

Headless runs never prompt. They start untrusted, without project configuration or
ambient instructions. On Windows, add `--sandbox off` (see Platforms).
`ego mcp-server` offers scoped runs to MCP clients. Run `ego --help` for every command.

The remaining command families are:

| Task | Commands |
| --- | --- |
| Continue or branch a session | `ego resume`, its alias `ego continue`, `ego fork`, `ego rewind` |
| Inspect or maintain sessions | `ego ls`, `ego rm`, `ego compact`, `ego export`, `ego checkpoint`, `ego usage` |
| Configure providers and tools | `ego auth`, `ego models`, `ego hooks`, `ego mcp` |
| Configure policy and defaults | `ego config`, `ego roots`, `ego rules`, `ego setup`, `ego doctor` |
| Manage checkouts and integrations | `ego worktree`, `ego build-recipes`, `ego a2a` |

## What you get

- **Permission modes** `plan`, `default`, `auto`, `accept-edits`, and `yolo`, chosen
  with `--mode` or `/mode`, or cycled with Shift+Tab. Confinement is separate
  (`--sandbox ro|workspace|adaptive|off`): even `yolo` keeps the sandbox.
  `/config perimeter` shows the effective boundary.
- **Sessions**: `ego ls`, `resume`, `fork`, `rewind`, `compact`, `export`, and
  `checkpoint`. Sessions nobody opened or wrote for 30 days are deleted
  automatically; change this with `sessions.retention_days`.
- **Isolated checkouts**: `ego --worktree BRANCH "..."` works in a retained Git
  worktree; `ego worktree cleanup` removes it only when it is clean and merged.
- **Providers**: Ollama, OpenAI, Anthropic, Gemini, OpenRouter, DeepSeek, Z.ai, Kimi,
  Amazon Bedrock, Google Cloud Vertex AI, and several subscription logins, all
  selected with `-m provider/model`. Keep several named accounts per provider and
  pick one with `--connection`. Capabilities a model lacks fail explicitly.
- **Tools**: read, search, edit, write, and execute; web fetch and web search; image
  input; parallel subagents; session goals; optional workspace memory and
  language-server lookups.
- **Integrations**: MCP servers (including OAuth login), ACP clients, A2A discovery,
  skills, and hooks.
- **Inspection**: `ego doctor`, `ego usage`, `ego config`, and `ego models`.

## Platforms

macOS confines with Seatbelt and defaults to the `adaptive` sandbox; Linux uses
Landlock and namespace backends and defaults to `workspace`. Windows has no
filesystem confinement: the `ro`, `workspace`, and `adaptive` sandboxes and
`--offline` are refused, and non-interactive starts need `--sandbox off`. An
interactive start asks once whether to continue unconfined. See
[troubleshooting](https://ego.tuicommander.com/docs/guides/troubleshooting.html).

## Documentation

- [Getting started](https://ego.tuicommander.com/docs/getting-started/) and [concepts](https://ego.tuicommander.com/docs/concepts/)
- [Interactive use](https://ego.tuicommander.com/docs/guides/interactive.html), [sessions](https://ego.tuicommander.com/docs/guides/sessions.html), [worktrees](https://ego.tuicommander.com/docs/guides/worktrees.html), and [automation](https://ego.tuicommander.com/docs/guides/automation.html)
- [Providers](https://ego.tuicommander.com/docs/guides/providers.html), [permissions](https://ego.tuicommander.com/docs/guides/permissions.html), and [tools](https://ego.tuicommander.com/docs/guides/tools.html)
- [Integrations](https://ego.tuicommander.com/docs/integrations/), the [CLI reference](https://ego.tuicommander.com/docs/reference/), and [configuration](https://ego.tuicommander.com/docs/reference/configuration.html)

## Building from source

```sh
cargo build --release
cargo test --workspace
```

The binary is `target/release/ego`. The user guide in `docs/` is an mdBook project.

## License

εgo is licensed under Apache-2.0 (see `LICENSE`). Releases up to and including 0.4.1
were MIT-licensed. Vendored and third-party components keep their own licenses; see
`docs/THIRD_PARTY_LICENSES.md`.
