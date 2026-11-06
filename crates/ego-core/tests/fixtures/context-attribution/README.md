# Recorded long-session tool payload fixture

Source: local session `01a19c78-8f01-7498-9c94-7b3e7134dc19`, recorded on
2026-11-02, 2,660 journal records and 105 tool calls. Captured read-only with jq.
This fixture retains all 105 starts and the 88 builtin completions. Foreign
completions are excluded to avoid retaining provider/host metadata. Their calls
therefore have explicitly unknown results; they are not zero-sized results.

Arguments are replaced by opaque JSON strings of the original UTF-8 byte length.
Each result text is replaced by a repeated SHA-256 digest at its original byte
length. Equal source payloads retain equal replacements. Process stream labels,
envelopes, event ordering, tool names and builtin output provenance are preserved.
Full-output references are removed; their content was not loaded into context.
No original command, path, source text or tool-output text remains.

This is an observed size/order fixture, not executable commands or a valid read
argument fixture. Separate domain cases test parsed read arguments and duplicates.
No provider call is involved. The fixture is not written back to the source journal.
