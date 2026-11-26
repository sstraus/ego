---
name: claude-code-wire-diff
description: After an authorized Claude Code update, compare the new native wire requests with the previous version for billing fingerprint, CCH seed and exclusions, OAuth identity headers, and streaming responses. Use for version-specific reverse-engineering checks, not routine login or automatic updates.
---

# Claude Code wire diff

Run on request after a Claude Code update. This skill does **not** authorize an update, paid requests, automatic monitoring, or changes to ego's default headers. Use current native bytes as the oracle; never change a version string in the old Python verifier and assume the new binary behaves the same.

## Baseline

- Read `docs/internals/2026-10-26-anthropic-auth-billing-wire.md` and, when available, `../orchestrator/research/archive/claude-code-billing/{README.md,verify.py}`. The archived verifier is pinned to an older version and implements only the hash exclusions exercised in its captures.
- Record `claude --version`, resolved executable path, binary SHA-256, previous captured version, and provenance. If the version is unchanged, report that and stop rather than claiming an update check.
- Before credential-bearing traffic, confirm the operator's authorization for the bounded calls and private captures. Use an isolated Claude config, short deterministic prompts, disabled tools and MCP, loopback proxy, timeouts, and a spend ceiling where supported. Keep raw bodies and session identifiers in a private ignored directory; do not log token bytes, token prefixes, or token digests. Decide whether to retain or remove evidence explicitly. Never silently disable upstream TLS verification: record a separately authorized, one-run exception when needed.

## Check the new version against wire bytes

1. Capture an **unmodified native** request and response. Preserve the exact serialized request body, not a reserialized JSON copy. Record non-secret header names and values, status, and decoded SSE event types. If comparing ego, test actual `Authorization` equality in memory and record only Booleans.
2. Diff native identity between versions: `user-agent`, `x-app`, `anthropic-beta` as both an **ordered string** and an unordered set, all other non-secret header names/values, JSON fields, billing-block field order, model, token ceiling, session and turn metadata. The `x-anthropic-billing-header: ...` marker is **text inside the first `system` block**, not an HTTP header. Distinguish moved beta markers from added or removed ones.
3. Recompute the version suffix from the original first user prompt, not necessarily the first serialized text block. Check the old candidate's JavaScript UTF-16 code-unit selection at positions 4, 7, and 20 (missing values become `0`), salt, UTF-8/SHA-256, and the **new** binary version. Compare with native `cc_version`. A three-hex match on one prompt is weak evidence; use a second independent prompt before claiming the algorithm still reproduces the version.
4. Restore only the native CCH's five bytes to `00000` in the captured body. Verify XXH64 with known vectors; check the prior seed, low-20-bit formatting and byte exclusions against the **original serialization**, without changing bytes sent to the server. Model-value and `max_tokens` exclusions were exercised in the prior captures; do not assume behavior for `fallbacks`, `fallback_credit_token`, reordered or final fields unless captured. Check at least two distinct native bodies, including a holdout. Matching output supports “old seed and normalization reproduce the observed bodies,” **not** unique seed recovery. If either candidate fails, stop ego identity simulation, isolate serialization/exclusion differences, and investigate before proposing a changed seed. Binary inspection can corroborate but cannot replace a wire holdout.
5. Only after the native checks pass, optionally try an **invocation-scoped Python proxy addon** with ego. Compute CCH from ego's own final body bytes and use the newly observed native version/headers; do not copy native CCH or modify product defaults. Keep credential equality, request differences, HTTP outcomes, and SSE comparisons separate. Compare streams only when both return usable SSE. A 429 does not identify its cause without a controlled comparison.

## Report and stop

Write a dated Markdown report under `docs/internals/`: exact old/new versions and binary hashes; header diff including beta order; version suffix and CCH checks per independent body; HTTP and stream evidence; capture directory and handling; TLS trust caveat; unknowns and verification commands. Label each claim **changed**, **unchanged for tested shapes**, or **not tested**. Run the relevant documentation checks. Do not automatically retry refusals, update the binary, alter ego auth defaults, or schedule future runs. Stop when the authorized sample is exhausted.
