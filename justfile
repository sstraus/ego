# The gate, in one place.
#
# `.github/workflows/ci.yml` runs these recipes rather than restating their
# commands, so there is exactly one definition of what "the gate passed" means.
# A change to a flag here changes CI in the same commit, and a green local run
# and a green CI run are the same claim.
#
# On this machine `cargo` resolves to the mbx shim when
# `~/Library/Application Support/mbx/bin` is on PATH, which is how concurrent
# builds share one artifact cache instead of holding every core. Nothing here
# depends on that: the recipes call `cargo`, and the shim is a PATH decision.
#
# Note on comments below: `just` takes the single comment line immediately above
# a recipe as its `--list` description. Anything longer is separated by a blank
# line so that `just --list` stays readable.

# List the recipes rather than running something.
default:
    @just --list

# Everything CI checks, cheapest failure first.
gate: fmt-check lint test

# Rewrite formatting in place.
fmt:
    cargo fmt --all

# Fail if anything is unformatted.
fmt-check:
    cargo fmt --all -- --check

# The workspace already denies `unsafe_code` and all of clippy in
# `[workspace.lints]`. `-D warnings` is what makes the remaining lint-level
# warnings fail the run rather than scroll past in a log nobody opens.

# Clippy over every target, warnings fatal.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# `cargo test` and not `cargo nextest`, because this recipe is the CI gate too
# and a runner should not need a second tool installed to reproduce it.

# The full suite, the way CI runs it.
test:
    cargo test --workspace

# Local iteration only, deliberately not part of `gate`: nextest's
# process-per-test model is what makes it fast and also what makes it a
# different execution model from the one CI reports on.

# Fast local run, optionally filtered to one test name.
t filter="":
    cargo nextest run --workspace {{ if filter == "" { "" } else { "-E test(" + filter + ")" } }}

# Kept out of `gate` because it depends on a fetched advisory database, so it
# fails for reasons that have nothing to do with the commit under test.

# Licence and advisory policy from deny.toml.
audit:
    cargo deny --all-features check

# Every live check is `#[ignore]`d *and* gated on `EGO_SMOKE` naming its
# provider, so a bare `cargo test` cannot trigger one. `ollama` is the only
# provider that is free and needs no credential, which is why it is the default
# and the only one CI ever runs.

# Live provider check; costs money for any provider but ollama.
smoke providers="ollama":
    EGO_SMOKE={{ providers }} cargo test -p ego-cli --test smoke -- --ignored

# `check` and not `test`: this answers "does the promised toolchain compile
# this?", which is all `rust-version` claims.

# Compile the workspace with the MSRV the manifest promises.
msrv:
    rustup run `just _msrv` cargo check --workspace --all-targets

# The single source of the MSRV, read by `msrv` and by the CI job so a version
# pinned in two files cannot disagree with itself.

# Print rust-version from Cargo.toml.
_msrv:
    @grep -m1 '^rust-version' Cargo.toml | sed -E 's/.*"(.+)".*/\1/'
