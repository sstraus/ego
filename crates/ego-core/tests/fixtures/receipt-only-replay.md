# Receipt-only replay fixture

Source: a copy of session `00000000-0000-4000-8000-0000000100a1`, recorded on
2000-01-18 and inspected on 2000-01-24. The JSON file retains the six durable
run transactions in source order. The three accepted prompts are all `ciao`;
all three turns failed before a semantic user prompt or assistant reply.

Capture identities, policy fingerprints and dates are synthetic; time deltas and replay relationships are preserved. The workspace root is `/workspace`. The fixture omits the
header, semantic confinement observations, manifest, evidence blobs, instruction
paths, configuration, locks, and later load-time observations. It contains no
credentials or personal paths. The test reads the same `SessionEntry` payloads
that the lifecycle store supplies; it does not emulate the JSONL adapter.
