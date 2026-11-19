# Open-source dependency licenses

Ego is distributed under the [Apache-2.0 license](../LICENSE).
The [dependency inventory](third-party-licenses.csv) records third-party package
versions and declared licenses. `Cargo.lock` pins the source dependency graph.
Third-party material retains its own license.

## Source code included in this repository

| Component | Declared license | License text |
| --- | --- | --- |
| `vendor/reedline` | MIT | [`vendor/reedline/LICENSE`](../vendor/reedline/LICENSE) |
| `vendor/process-wrap` | Apache-2.0 OR MIT | [`LICENSE-APACHE`](../vendor/process-wrap/LICENSE-APACHE), [`LICENSE-MIT`](../vendor/process-wrap/LICENSE-MIT) |
| Notebook code derived from nbformat | BSD-3-Clause | [`nbformat.LICENSE`](../crates/ego-tools/src/tools/nbformat.LICENSE) |

Ego is licensed under Apache-2.0. See the repository's
[LICENSE](https://github.com/sstraus/ego/blob/main/LICENSE) for the full terms.
The bundled components above retain their own licenses. Review their source
and binary distribution requirements when redistributing ego.

The CSV covers Cargo registry packages only. It does not assert that tools
used to build or test the project, external model providers, or operating-system
libraries are bundled with a release.
