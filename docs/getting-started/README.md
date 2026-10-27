# Getting started

Use εgo to work with a model in your current repository. You control the model,
workspace trust, tool permission, and confinement. A model credential alone does
not grant tools access to your machine.

Release `0.4.0` is being prepared, not published. See the
[release notes](../release-notes/0.4.0.md) and
[installation overview](../../README.md#install) for the planned binary assets
and checksum procedure. The source procedure below works without publication.

## Build and install from source

The workspace requires Rust 1.95 or newer (`Cargo.toml`). Install from a complete
checkout; the CLI uses a repository-local patched Reedline in `vendor/reedline`.
The repository has not published a stable binary or crates.io package.

```sh
cargo install --locked --path crates/ego-cli --root "$PWD/.ego-bin"
export PATH="$PWD/.ego-bin/bin:$PATH"
ego --help
```

`.ego-bin` is a local install prefix, not configuration or session storage. Remove
that prefix when you no longer want the installation. Do not commit it. On this
project's development Mac, prefix PATH with the existing mbx shim directory and
check `mbx doctor` before Cargo; mbx chooses build targets and preserves caches.

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
usable provider has a mapped default. Otherwise it opens the model picker.
Headless startup always needs an explicit or configured model; it does not choose
or save one on your behalf.

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
