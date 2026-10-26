# Build and maintain the guide

The user documentation is one set of Markdown files. It can be read in the
checkout or built as an mdBook, with a chapter sidebar, search, and print view.
`SUMMARY.md` owns book navigation. Internal research and dated decisions remain
in `internals/`; they are not user-guide chapters.

## Requirements

Use Python 3 and mdBook 0.5.4. These are documentation tools, not
runtime dependencies of ego. Install them with your normal development-tool
workflow if they are absent; the documentation build does not install tools.

From the repository root:

```sh
python3 tools/docs/check-links.py
PYTHONDONTWRITEBYTECODE=1 python3 tools/docs/test-book-source-paths.py
mdbook build docs
python3 tools/docs/check-book.py docs/book
```

The HTML output is `docs/book/`, which is ignored by Git. To preview locally,
run `mdbook serve docs --hostname 127.0.0.1 --port 3000`, open the displayed
loopback URL, and stop the server when finished. To place a build elsewhere,
use `mdbook build docs --dest-dir /absolute/output/directory`.

Building a book does not publish a website. No hosted documentation URL or
remote deployment is required by this configuration.

## Add or update a page

1. Verify behavior in the parser, implementation, and relevant tests. Do not turn
   a design proposal into a claim about an implemented feature.
2. Update the existing task-oriented page instead of creating a parallel guide.
3. Add a new user page to `SUMMARY.md` and the relevant index.
4. Use relative Markdown links. Keep source references useful in the checkout.
5. Run the documentation checks and build above. Inspect the resulting navigation
   and the pages you changed. Application suites are not a documentation gate.

The book preprocessor keeps chapter links and bundled assets navigable. Citations
to repository source or documents outside the book appear as explicit repository
paths in HTML, not broken links to files that mdBook does not publish. The source
Markdown and its links are not rewritten. Code examples remain literal.

## Command reference and help snapshots

`reference/cli.md` is generated from the parser. Do not edit it by hand. The
reference tests in `crates/ego-cli/tests/reference.rs` describe how to regenerate
it with `EGO_UPDATE_ARTIFACTS=1`; follow the repository's Cargo build policy.

`reference/help.md` is a dated capture from an actual binary, not a second
command specification. Recapture it when the command surface changes, and name
the binary version and capture date. Read `ego COMMAND SUBCOMMAND --help` for the
installed binary's exact arguments. Help-only captures do not contact a provider.
To refresh the snapshot from a built executable:

```sh
python3 tools/docs/capture-help.py /absolute/path/to/ego
```

The helper uses command paths from the generated reference, records binary version,
date, and SHA-256, and refuses to write if the executable changes during capture.

README is the product entry point, not the complete command manual. Keep its
version/status, first-run examples, and links consistent with these guides.

## Prepare release documentation

The workspace version is in `[workspace.package]` in `Cargo.toml`. Workspace
packages inherit it. Let Cargo refresh their lockfile versions; do not edit
`Cargo.lock` by hand. A release preparation can select a new minor version;
`0.3.0` is the current local preparation, not a publication claim.

Write notes in `release-notes/VERSION.md`, grouped by user-visible features,
fixes, and breaking changes. Include story IDs and verify the changes against
Git history and source. Keep dated captures and external dependency versions
unchanged; they are evidence, not release declarations. Add the notes to
`SUMMARY.md` and link them from README.

The tag workflow reads that exact notes file and refuses a missing or empty file.
A documentation build does not authorize a tag, push, release, tap update, or
installed-binary replacement. Follow the authorization gates in
[the release-flow report](internals/649-release-flow.md#publication-steps-require-maintainer-authorization).
