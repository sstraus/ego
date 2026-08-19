# ego

A coding agent harness written in Rust.

**Status: architecture proposed.** This repository contains research,
specification and the proposed design — no code yet. `ARCHITECTURE.md` resolves
the eight open questions of `SPEC.md` §4; `CLI.md` defines the command
language. Both are pending maintainer review.

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

