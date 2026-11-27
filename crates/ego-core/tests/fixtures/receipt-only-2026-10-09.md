# Receipt-only replay fixture

Source: a copy of session `972a33b8-dbf8-503e-ad67-e32065c0efc3`, recorded on
2026-10-09 and inspected on 2026-10-15. The JSON file retains the six durable
run transactions in source order. The three accepted prompts are all `ciao`;
all three turns failed before a semantic user prompt or assistant reply.

Only the workspace root was changed to `/workspace`. The fixture omits the
header, semantic confinement observations, manifest, evidence blobs, instruction
paths, configuration, locks, and later load-time observations. It contains no
credentials or personal paths. The test reads the same `SessionEntry` payloads
that the lifecycle store supplies; it does not emulate the JSONL adapter.
