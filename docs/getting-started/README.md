# Getting started

Use εgo to work with a model in your current repository. You control the model,
workspace trust, tool permission, and confinement. A model credential alone does
not grant tools access to your machine.

You can also read this page in the [hosted user guide](https://sstraus.github.io/ego/getting-started/).

## Download a binary

Choose an executable and its checksum from
[GitHub Releases](https://github.com/sstraus/ego/releases). Release assets are
available only after the release workflow succeeds; a tag alone is not proof
of a download.

| Platform | Executable |
| --- | --- |
| macOS Apple Silicon | `ego-macos-arm64` |
| macOS Intel | `ego-macos-x64` |
| Linux x86_64 | `ego-linux-x64` |
| Linux ARM64 | `ego-linux-arm64` |
| Windows x86_64 | `ego-windows-x64.exe` |

Linux binaries require GLIBC 2.35 or newer. Target-triple `.tar.gz` archives
(`.zip` on Windows) and `SHA256SUMS` are also published. Each archive includes
Bash, Zsh, Fish, PowerShell, and Elvish scripts in `completions/`.

For example, on macOS Apple Silicon:

```sh
asset=ego-macos-arm64
base=https://github.com/sstraus/ego/releases/latest/download
curl -fLO "$base/$asset"
curl -fLO "$base/$asset.sha256"
shasum -a 256 -c "$asset.sha256"
mkdir -p "$HOME/.local/bin"
install -m 755 "$asset" "$HOME/.local/bin/ego"
```

On Linux, choose the matching asset and use `sha256sum -c`. On Windows, compare
the downloaded checksum with `Get-FileHash -Algorithm SHA256`, then rename the
executable to `ego.exe` in a directory on `PATH`. Add `~/.local/bin` to `PATH`
on macOS or Linux if needed. For Homebrew installation, see the
[installation overview](../../README.md#install).

## Build and install from source

The workspace requires Rust 1.95 or newer (`Cargo.toml`). Install from a complete
checkout; the CLI uses a repository-local patched Reedline in `vendor/reedline`.
Workspace crates are not published to crates.io. Binary downloads depend on
the release workflow completing successfully; tagging is not proof of a download.

```sh
cargo install --locked --path crates/ego-cli --root "$PWD/.ego-bin"
export PATH="$PWD/.ego-bin/bin:$PATH"
ego --help
```

`.ego-bin` is a local install prefix, not configuration or session storage. Remove
that prefix when you no longer want the installation. Do not commit it.

## Choose a model

With a running Ollama daemon and an installed completion model:

```sh
ego models --provider ollama --refresh
ego config set 'roots=[]'
ego -m ollama/gemma4:e4b-mlx --thinking off --sandbox workspace
```

`roots=[]` clears configured extra roots; `--sandbox workspace` makes the example's
confinement explicit instead of accepting the platform default. Inspect
`/config perimeter` for admitted roots and backend facts.

Replace the slug with a model your catalogue lists. An embedding-only model is
not a completion model. `--thinking off` is explicit; a different model may
require another supported effort. A missing capability remains unknown and can
refuse a request. Hosted credentials and subscription login are described in
[providers](../guides/providers.md).

Type “Explain the main entry points in this repository.” At interactive startup,
εgo asks before trusting project configuration and ambient instructions. Answer
for the workspace you intended to open. `/help` lists controls; `/tools` shows
what this session offers; `/config perimeter` shows its admitted boundary.
`Ctrl+D` or `/quit` exits without erasing the durable session.

Interactive startup can select and persist a first-run default when exactly one
usable provider has a mapped default. Otherwise it opens the model picker, which
groups models by provider and names the sign-in remedy above each unavailable
provider. Headless startup with exactly one usable provider uses
that provider's default for the run, names it on stderr, and does not save it.
With no usable provider, several, or no mapped default, headless startup refuses
and lists the `--model` remedies. `ego setup` shows which providers are ready.

## Terminal, pipeline, or host

`ego` opens the interactive driver. `ego run` is headless even when stdin is a
terminal. `ego resume` without a prompt opens the interactive session picker or
resumes the named session; a prompt or `-` keeps it headless.

```sh
printf '%s\n' 'Reply with one sentence about what a coding harness does.' | \
  ego run -m ollama/gemma4:e4b-mlx --thinking off --no-tools --timeout 30s --output json -
ego ls --output json
ego usage --json
```

The pipeline leaves a durable session. Its stdout is JSON; diagnostics go to
stderr. It never asks for trust or permissions. It starts untrusted and bare;
use explicit instructions and deliberate rules for scripts that need effects.

For an ACP host, configure its agent executable as `ego` with argument `acp`.
The host owns the protocol connection and permission UI. See
[ACP and TUICommander](../integrations/acp.md); do not run the stdio server as an
ordinary interactive chat command.

Next: [interactive use](../guides/interactive.md), [automation](../guides/automation.md),
and [sessions](../guides/sessions.md).
