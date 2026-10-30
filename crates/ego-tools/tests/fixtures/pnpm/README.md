# Synthetic pnpm evidence

These files preserve the denial operations, paths and outcomes observed with
pnpm 11.15.1. They are privacy-safe examples, not unchanged host logs.

Calendar and encoded clocks, process and thread IDs, user IDs, lock owners,
mtimes, elapsed measurements, temporary names and lock fingerprints are
synthetic. Equal values remain equal across files. Distinct identities remain
distinct. Clock ordering is preserved, but original time intervals are not.
Elapsed examples preserve successful baselines below the ten-second deadline
and timed-out witnesses above it. Lock fingerprints with a stated path are
SHA-256 hashes of that example path. Other fingerprints are synthetic hashes.

The dead-owner and unchanged-owner observations, live/dead distinctions, denial
order, duplicate reports, permission modes and lock-presence relationships are
preserved. These fixtures document observed behavior; they do not establish
that a current installed pnpm or native macOS run passes.
