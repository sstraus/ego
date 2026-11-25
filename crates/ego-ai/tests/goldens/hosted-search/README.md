# Published hosted-search examples

These files record data examples from the providers' public documentation, read on 2026-10-16. They are documentation fixtures, not live API recordings. No API key, paid request, or external search was used.

- `openai-output.json`: the output array under **Output and citations** at https://developers.openai.com/api/docs/guides/tools-web-search . The published truncated URL, title, text, and annotation offsets are retained. The example does not include token usage or SSE framing.
- `anthropic-response.json`: **Response** at https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool . JSON comments are removed. The published truncated encrypted strings and cited text are retained, not replaced with invented captures.

`hosted_search.rs` puts the recorded data into documented streaming frames and splits those bytes at different transport boundaries. Its terminal OpenAI token counts and Anthropic `pause_turn` frame exercise the streaming contract; they are explicit test inputs, not claims about the example's observed transport. Production provider content is assembled once at the wire boundary. The opaque JSON blob and every encrypted string are then persisted and replayed without edits. Byte-exact replay refers to that captured blob, not the whitespace of the original SSE envelope.

The examples prove offline encoding, decoding, accounting dimensions, and replay behavior only. They do not promote a candidate provider, establish per-model search availability, validate the truncated example URLs, or prove live conformance.
