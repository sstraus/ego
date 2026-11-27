# Build and maintain the guide

The [hosted user guide](https://ego.tuicommander.com/docs/) is published with the
website by the [website workflow](https://github.com/sstraus/ego/blob/main/.github/workflows/website.yml):
the site from `website/` at the root of `ego.tuicommander.com`, the guide under `/docs/`.
Readers do not need to build it. Contributors maintain one set of Markdown files;
`SUMMARY.md` owns the chapter sidebar. Internal research and dated decisions are
kept outside the published guide; they are not user-guide chapters.

## Requirements

Use Python 3 and mdBook 0.5.4. These are documentation tools, not
runtime dependencies of ego. Install them with your normal development-tool
workflow if they are absent; the documentation build does not install tools.

From the repository root:

```sh
python3 -B tools/docs/test-check-links.py
python3 tools/docs/check-links.py
PYTHONDONTWRITEBYTECODE=1 python3 tools/docs/test-book-source-paths.py
mdbook build docs
python3 tools/docs/check-book.py docs/book
```

The source-link checker requires Git and checks only tracked Markdown files in
its scope. Stage new pages before running it. Ignored local records, generated
files, and untracked drafts are excluded; tracked files remain checked even if
a later ignore rule matches them. A link to a missing target that Git ignores,
such as a local recording under `results/` or `.tmp/`, is skipped, because no
checkout contains it; a force-added file under an ignore rule is still checked.

The HTML output is `docs/book/`, which is ignored by Git. To preview locally,
run `mdbook serve docs --hostname 127.0.0.1 --port 3000`, open the displayed
loopback URL, and stop the server when finished. To place a build elsewhere,
use `mdbook build docs --dest-dir /absolute/output/directory`.

## Automated publication and Pages setup

The workflow runs on pushes to `main`, release tags matching `v*`, and manual
workflow dispatch. It downloads the official Linux mdBook **0.5.4** release
archive and verifies its recorded SHA-256 before extracting or running it. It
then runs the source-link and preprocessor checks, builds the book, and checks
the generated HTML. It copies `website/` to the artifact root and `docs/book/`
to `docs/`, and uploads both as one Pages artifact. A separate deployment job uses
`actions/deploy-pages`, the `github-pages` environment, and job-scoped
`pages: write` and `id-token: write` permissions. Deployments share the `pages`
concurrency group and do not cancel a deployment already in progress.

Before first publication, a repository administrator must approve and perform
these steps in the **public** `sstraus/ego` repository:

1. Open **Settings → Pages → Build and deployment** and select **GitHub Actions**
   as the source. Do not select branch-based publication.
2. Review the `github-pages` environment deployment rules. They must permit
   `main` and release tags matching `v*` for both workflow triggers to deploy.
3. Under **Custom domain**, enter `ego.tuicommander.com` (an Actions deployment
   ignores a `CNAME` file, so the setting is the only source) and enable **Enforce HTTPS** once the certificate exists. The DNS zone of
   `tuicommander.com` needs a `CNAME` record `ego` → `sstraus.github.io`.
4. After the workflow is present on `main`, run **website** manually
   or allow an authorized push or release tag to trigger it.
5. Check the build and deployment receipts and open
   <https://ego.tuicommander.com/> and <https://ego.tuicommander.com/docs/>.
   The site may return 404 until the first successful deployment. A local build alone does not prove publication.

These are publication steps, not prerequisites for reading or contributing.
Do not change repository settings or publish without maintainer authorization.
To check workflow syntax without deploying, run
`actionlint .github/workflows/website.yml` alongside the
local build above.

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
reference tests in `crates/ego-cli/tests/it/reference.rs` describe how to regenerate
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

## Prepare a release

The workspace version is in `[workspace.package]` in `Cargo.toml`. Workspace
packages inherit it. Let Cargo refresh their lockfile versions; do not edit
`Cargo.lock` by hand. A release preparation can select a new minor version.

`CHANGELOG.md` is the only change record and it is mandatory:

1. Every commit with an effect that a user, script, or host integration can
   observe adds an entry under `## [Unreleased]`, with its story ID.
2. To prepare version `VERSION`, move all `Unreleased` entries to a new
   `## [VERSION] - YYYY-MM-DD` section below the empty `Unreleased` heading.
   Group them as Added, Changed, Deprecated, Removed, Fixed, and Security. Mark
   breaking changes with `**Breaking:**`.
3. Verify the section against Git history and source. Every story ID in a
   feat, fix, or perf commit since the previous version must be listed, or be
   named in an `<!-- internal: ... -->` comment when it has no user-visible
   effect or an earlier section already lists it.
4. Run `just changelog-check` on the private history. It runs
   `scripts/check-changelog.py release --history` and refuses missing, undated,
   empty, or incomplete sections and leftover `Unreleased` entries. Pass
   `--since REF` to the script when the previous version was tagged from a later
   commit than its version bump.

Keep dated captures and external dependency versions unchanged; they are
evidence, not release declarations. Do not write new `release-notes/VERSION.md`
files: the files for `0.3.0` to `0.4.3` remain as an archive.

The tag workflow runs the same check without `--history`, because a public
squash snapshot carries no story IDs. It publishes the version section of
`CHANGELOG.md` as the release notes and refuses the release when the check fails.
A documentation build does not authorize a tag, push, release, tap update, or
installed-binary replacement. Each of those publication steps needs explicit
authorization from the project owner.
