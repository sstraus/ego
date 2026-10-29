# Contributing

Use Rust 1.95 or newer, Python 3.11 or newer, and
[just](https://github.com/casey/just). Keep the complete checkout, including
`vendor/`, when building. Vendored patches have their own change notes and licenses.

## Make a change

1. Read the relevant source, callers, and tests before changing behavior.
2. Keep the change focused. Preserve public output, protocol, and permission
   contracts unless the change explicitly updates them.
3. For a defect, add a regression test that fails for the reported behavior.
   Test observable results and important boundaries.
4. Update the user guide for changed commands or behavior. Keep generated
   references in sync; see [documentation maintenance](docs/maintaining.md).
5. Run the applicable checks and describe their scope in your pull request.

## Checks

```sh
just gate
cargo check --workspace --all-targets --locked
```

`just gate` checks formatting, the generated model catalogue, warnings-denied
Clippy, and workspace tests. CI runs the gate on Linux, macOS, and Windows and
also checks the declared minimum Rust version and dependency policy.

Use `cargo test -p CRATE TEST_FILTER` for focused iteration. Live provider tests
are opt-in and can incur charges. They are not required for a normal offline
change. Do not put credentials or private session records in tests.

To validate release packaging without building a binary:

```sh
ruby scripts/check-release-assets.rb
```

This check uses inert executable fixtures. It validates Unix packages and checks
Windows naming; ZIP execution requires the Windows release runner's `7z`.

## Model catalogue

The builtin catalogue is generated from the pinned snapshot and reviewed policy
in `crates/ego-cli/catalogue/`. Use Python 3.11 or newer:

```sh
python3 -B scripts/catalogue.py check
python3 -B scripts/catalogue.py generate
```

`fetch` contacts the upstream catalogue; review the resulting diff before
regenerating. Capabilities must come from supported provider contracts. Missing
prices and capabilities must not be invented.

## Releases

Release notes live in `docs/release-notes/VERSION.md`. The release workflow checks
that a `vVERSION` tag matches the workspace version and that its notes exist.
Tagging can publish assets and update the Homebrew tap. Do not use a publication
workflow as a local validation command.

## License and private information

Contributions are provided under the project's [MIT License](LICENSE). Preserve
third-party copyright and license notices. Do not commit API keys, access tokens,
personal paths, private host inventories, or local diagnostic output. Use
anonymized fixtures and record which fields were changed.
