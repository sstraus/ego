# Pinned Crossterm parser correction

Source: crates.io crossterm 0.29.0, upstream revision
`36d95b26a26e64b0f8c12edfe11f410a6d56a812`, MIT license in `LICENSE`.
The source comes from the existing Cargo registry copy. Cargo cache markers,
the package lockfile, and upstream CI configuration are not included.

On 2026-10-17, docs.rs `/crate/crossterm/latest` reports 0.29.0 as the
latest release. Upstream `master` still has the same `Parser::advance`
behavior in `src/event/source/unix/{mio,tty}.rs`: when the parser sees two
Escape bytes it emits one Escape and clears both bytes.

A cancellation Escape immediately followed by a cursor-position reply can
arrive in one tty read. The second Escape belongs to the reply. Losing it
makes `[21;3R` ordinary composer text and makes the cursor query time out.
The local correction emits the first Escape and parses the second as the
start of its own sequence. Regression tests cover one read, every read
boundary, arrow keys, and repeated Escape keys. No public API changes.

The workspace patch applies the correction to Reedline and the CLI, which
share Crossterm input ownership. The existing 80x24 PTY cancellation test
keeps its assertions. The fixture does not delete cursor-response text.

DEFERRED (2026-10-17) — Remove this vendor copy when upstream ships the
fix in `src/event/source/unix/{mio,tty}.rs`, `Parser::advance` (upstream lines 198/247).
No upstream issue or pull request was filed.
