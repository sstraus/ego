# εgo

εgo is a coding agent for your terminal, scripts, and ACP clients. It is written
in Rust and works with local models, hosted APIs, and supported subscription
providers. It supplies file and process tools, durable conversations, permissions,
and operating-system confinement.

Choose the model. Review the permissions. Keep control of your workspace.

## Install

Release assets and the Homebrew formula become available when the first release
is published. You can build from source before publication.

### Homebrew

On macOS and Linux:

```sh
brew install sstraus/tap/ego
```

Homebrew installs Bash, Zsh, and Fish completions in its standard completion
directories. PowerShell and Elvish scripts are available under
`$(brew --prefix ego)/share/ego/completions/`.

### Direct download

Download an executable and its SHA256 file from
[GitHub Releases](https://github.com/sstraus/ego/releases).

| Platform | Asset |
| --- | --- |
| macOS Apple Silicon | `ego-macos-arm64` |
| macOS Intel | `ego-macos-x64` |
| Linux x86_64 | `ego-linux-x64` |
| Linux ARM64 | `ego-linux-arm64` |
| Windows x86_64 | `ego-windows-x64.exe` |

Linux binaries require GLIBC 2.35 or newer. Target-triple `.tar.gz` archives
(`.zip` on Windows) remain available, together with `SHA256SUMS`. Each archive
includes a `completions/` directory with Bash, Zsh, Fish, PowerShell, and Elvish
scripts. Use the archive if you need completion files with a direct download.
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

On Linux, choose the matching asset and use `sha256sum -c`. On Windows, download
`ego-windows-x64.exe` and its checksum. Compare with PowerShell
`Get-FileHash -Algorithm SHA256`, then rename the executable to `ego.exe` in a
directory on `PATH`. Add `~/.local/bin` to `PATH` on macOS or Linux if needed.

### From source

Use Rust 1.95 or newer and a complete checkout, including `vendor/`:

```sh
cargo install --locked --path crates/ego-cli --root "$PWD/.ego-bin"
export PATH="$PWD/.ego-bin/bin:$PATH"
ego --help
```

Alternatively, `scripts/local-release.sh` builds and atomically installs to
`~/.local/bin/ego`. This helper requires Python 3 and leaves existing sessions
running. Workspace crates are not published to crates.io.

## Quick start

With a running [Ollama](https://ollama.com) daemon:

```sh
ollama pull qwen3
ego models --provider ollama --refresh
ego config set 'roots=[]'
ego -m ollama/qwen3 --thinking off --sandbox workspace
```

Type a first prompt such as “Explain the main entry points in this repository.”
The model name above is an example: use one listed by your daemon.
`ego config set 'roots=[]'` clears configured extra roots. The example also selects
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
`ego rules`, `ego worktree`, and `ego mcp-server`.

For a non-interactive run:

```sh
printf '%s\n' 'Summarize what a coding agent does in one sentence.' | \
  ego run -m ollama/qwen3 --thinking off --no-tools --timeout 30s --output json -
```

Headless runs do not prompt for trust or permissions. They start untrusted and
bare by default. See [automation](docs/guides/automation.md) before enabling effects.

## Commands

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
- [ACP, MCP, skills, and hooks](docs/integrations/README.md)
- [Architecture](docs/architecture.md) and [open-source licenses](docs/THIRD_PARTY_LICENSES.md)

| Commands | Purpose |
| --- | --- |
| `ego run` | Run a prompt headlessly |
| `ego resume`, `ego continue` | Continue durable work |
| `ego fork`, `ego compact` | Branch or reduce conversation context |
| `ego ls`, `ego rm`, `ego usage` | Inspect stored sessions, remove one, or inspect usage |
| `ego export`, `ego checkpoint` | Export a session or inspect file recovery points |
| `ego models`, `ego auth` | Inspect models and manage provider login |
| `ego config`, `ego roots`, `ego rules` | Manage configuration and authority |
| `ego hooks`, `ego mcp` | Inspect hooks and manage MCP connections |
| `ego doctor`, `ego setup` | Diagnose the environment and review setup proposals |
| `ego acp`, `ego a2a`, `ego mcp-server` | Serve host protocols |
| `ego orchestrator`, `ego worktree` | Inspect orchestration or retained worktrees |

## Configuration and features

- Use `ego models` to inspect available models and capabilities.
- Use `ego config ls --effective` to inspect resolved configuration.
- Resume, fork, compact, and export durable sessions.
- Read, search, edit, write, and execute through permission-controlled tools.
- Bound runs by tokens or money, turns, and time.
- Use JSON, JSONL, and schema-validated output in scripts.
- Connect MCP tools, load skills and hooks, or run `ego acp` behind an ACP host.
- Use `ego --worktree feat/task "implement the task"` for an isolated Git checkout.

Permission modes are `plan`, `default`, `auto`, `accept-edits`, and `yolo`.
Permission mode and operating-system confinement are separate. A relaxed mode
does not bypass a sandbox or administrator limits.

macOS uses Seatbelt. Linux uses its supported confinement backends. An unavailable
guarantee is refused rather than silently weakened. Windows supports execution,
transport, and private state, but `workspace`, `ro`, and `--offline` confinement
are not supported. See [troubleshooting](docs/guides/troubleshooting.md).

## Documentation

Start with the [user guide](docs/README.md) or `ego --help`.

- [Getting started](docs/getting-started/README.md)
- [Interactive use](docs/guides/interactive.md), [sessions](docs/guides/sessions.md), and [worktrees](docs/guides/worktrees.md)
- [Configuration](docs/reference/configuration.md) and [CLI reference](docs/reference/README.md)
- [Permissions](docs/guides/permissions.md), [providers](docs/guides/providers.md), and [tools](docs/guides/tools.md)
- [ACP, MCP, skills, hooks, and orchestration](docs/integrations/README.md)
- [Architecture](ARCHITECTURE.md) and [contributing](CONTRIBUTING.md)
- [Release notes](docs/release-notes/0.4.0.md)

Build the guide with mdBook 0.5.4 and Python 3: `mdbook build docs`.
See [documentation maintenance](docs/maintaining.md).

## License

εgo is licensed under the [MIT License](LICENSE). Vendored dependencies and other
third-party material retain their own licenses; see
[third-party licenses](docs/THIRD_PARTY_LICENSES.md).
