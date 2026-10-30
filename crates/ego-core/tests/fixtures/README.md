# Recorded replay fixtures

Recorded fixtures in `ego-core`, `ego-cli`, and `ego-mcp` retain captured protocol and event behavior. Their capture identities and provenance are synthetic: session, event, turn, item, parent, generation, request, lease, executor, message, provider-call and run identifiers do not name the original captures. Capture dates and absolute timestamps are synthetic too. Equal identifiers stay equal, distinct identifiers stay distinct, and event ordering and elapsed times are preserved.

The receipt-only replay keeps the original prompts, failure outcomes, revisions and lease relationships. Its policy fingerprints are synthetic but preserve the observed policy mismatch. The shared constructed CLI fixture session constant and constructed codec fixtures are not captured identities.

Modified durable JSONL records carry checksums over their new sequence and raw payload bytes. Compaction seed provenance uses `seed_event_sha256`, computed from compact UTF-8 JSON of the synthetic `seed_event`; it does not claim to hash an omitted original journal. Tool-output references hash the retained public text. Public SDK source revisions and protocol/model version labels remain unchanged.
