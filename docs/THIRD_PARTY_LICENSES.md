# Open-source dependency licenses

This inventory describes the Rust dependency graph in `Cargo.lock`. The
[crate-level list](third-party-licenses.csv) records the name, version, and
upstream-declared license expression for each registry package resolved with
all workspace features. An `OR` expression offers a choice of licenses; it
does not mean that all alternatives apply at once. An `AND` expression requires
more than one license. The list is an inventory, not a replacement for upstream
license texts or legal advice.

To refresh the list, run `cargo metadata --locked --offline --format-version 1
--all-features` and export each registry package's `name`, `version`, `license`,
and `source`. Run `cargo deny --offline --locked --all-features check licenses`
to check the selected dependency graph against `deny.toml`. The inventory in
this checkout covers 541 registry package versions. The offline license check
passed for this graph on 2026-10-18. The advisory check also passed against the
cached database; it did not fetch new advisories. Refresh the inventory and
checks before a release or a dependency change.

The allow-list in [`deny.toml`](../deny.toml) contains these license
identifiers: `0BSD`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`,
`BSD-2-Clause`, `BSD-3-Clause`, `BSL-1.0`, `CC0-1.0`,
`CDLA-Permissive-2.0`, `ISC`, `MIT`, `MIT-0`, `Unicode-3.0`, `Unlicense`,
and `Zlib`. Some packages declare more than one option. The upstream
expressions, not only the allowed identifiers, are in the CSV file.

## Source code included in this repository

| Component | Declared license | License text |
| --- | --- | --- |
| `vendor/reedline` | MIT | [`vendor/reedline/LICENSE`](../vendor/reedline/LICENSE) |
| `vendor/process-wrap` | Apache-2.0 OR MIT | [`LICENSE-APACHE`](../vendor/process-wrap/LICENSE-APACHE), [`LICENSE-MIT`](../vendor/process-wrap/LICENSE-MIT) |
| Notebook code derived from nbformat | BSD-3-Clause | [`nbformat.LICENSE`](../crates/ego-tools/src/tools/nbformat.LICENSE) |

Workspace manifests declare `MIT`, but the repository does not yet contain a
root license text. Add the intended `LICENSE` file before publishing: the
manifest declaration alone is not the complete license notice. Also review
source and binary distribution requirements for the bundled code above.

The CSV covers Cargo registry packages only. It does not assert that tools
used to build or test the project, external model providers, or operating-system
libraries are bundled with a release.
