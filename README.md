# εgo

A compact, line-oriented coding agent harness written in Rust.

The working binary provides interactive sessions, headless runs, durable
resume/listing, subscription login and an ACP server. Ollama and OpenAI remain
selectable directly; current subscription profiles use `ego auth`. Commands
whose subsystem has not landed are refused by name instead of being advertised
as working.

Available entry points are `ego [PROMPT]`, `ego run`, `ego resume`, `ego ls`
and `ego acp`, plus `ego auth` for subscription credentials. Promptless
`ego resume [ID]` re-enters the interactive driver; a resume carrying a prompt
or `-` stays headless.

For a subscription profile, log in before selecting its model:

```sh
ego auth login openai-codex
ego -m openai-codex/gpt-5.3-codex
```

## Ollama quickstart

Start Ollama, install a model, then open an interactive εgo session:

```sh
ollama pull qwen3
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

maintainer's alter ego. Availability verified on 2026-08-19:

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

