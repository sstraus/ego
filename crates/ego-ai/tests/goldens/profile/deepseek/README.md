# DeepSeek Chat fixtures

Provenance: `VendorSchemaDerived`. No DeepSeek credential exists for this
repository, so no fixture here was recorded from a live server. Each file is
assembled from the published contract, read 2026-11-14:

- Chat Completions reference:
  <https://api-docs.deepseek.com/api/create-chat-completion>
- Thinking mode guide (effort mapping, reasoning replay with tools):
  <https://api-docs.deepseek.com/guides/thinking_mode>
- Error codes: <https://api-docs.deepseek.com/quick_start/error_codes>
- Models and pricing (base URL, model names):
  <https://api-docs.deepseek.com/quick_start/pricing>

The conversation text, call ids and reasoning strings come from the tool-call
sample of the thinking mode guide. The chunk shape (`logprobs: null`, usage on
the last content chunk with a non-null `finish_reason`, no usage-only chunk)
comes from the streaming example of the reference.

Two test modules read these files. `crates/ego-ai/tests/it/conformance.rs`
runs the profile checks on `request/canonical.json`, `stream/text.sse`,
`stream/tool_calls.sse`, `usage/` and `error/` (profile `DEEPSEEK` in
`tests/profiles/mod.rs`). `crates/ego-ai/tests/it/deepseek.rs` reads the
other request bodies, the interruption transcripts and the error statuses.

| Fixture | Content | Bug it catches |
| --- | --- | --- |
| `request/canonical.json` | URL, header names and body of the profile request: thinking disabled, `max_tokens` 16 | The row sends to another endpoint, drops its credential header, or puts a key value into a fixture |
| `request/reasoning_tools.json` | `max_tokens`, `thinking` enabled, `reasoning_effort` `high`, tools; no `max_completion_tokens`, no `parallel_tool_calls` | The ceiling goes in a field DeepSeek does not read, so the output cap is lost; a field outside the schema is sent |
| `request/thinking_off.json` | `thinking` disabled, no `reasoning_effort` | Off leaves the default thinking active |
| `request/tool_result_continuation.json` | Every earlier assistant turn carries its `reasoning_content`, also the turn without a tool call, before a later user turn | A tool continuation without the earlier reasoning gets the documented `400` |
| `stream/text.sse` | Text, `finish_reason: stop`, usage with the cache hit and miss counts under `prompt_tokens_details` (the schema placement) | Usage on the finish chunk is dropped; `Stop` is not last |
| `stream/tool_calls.sse` | `reasoning_content` deltas, split tool-call arguments, `finish_reason: tool_calls`, usage with the hit and miss counts at the top level (the example placement) beside `cached_tokens` | Argument fragments are joined wrongly; the reasoning is not kept as DeepSeek replay state; the cache hit is counted twice or not at all |
| `stream/insufficient_resource.sse` | `finish_reason: insufficient_system_resource` | A server interruption reads as a clean stop, or is not retried |
| `stream/aborted.sse` | `finish_reason: aborted` | The same, for an interrupted generation |
| `usage/text.0.json`, `usage/tool_calls.0.json` | The usage chunk of each transcript | The input parts do not add up to `prompt_tokens` |
| `error/401.json` | Wrong API key | A refused key is retried or not reported as refused |
| `error/402.json` | Insufficient balance | An empty balance becomes a retry loop |
| `error/422.json` | Invalid parameters | An invalid request is retried |
| `error/429.json` | Rate or concurrency limit | A rate limit loses its retry |
| `error/503.json` | Server overloaded | An overload loses its retry |

The error page documents statuses and causes but no body. The error fixtures
use the family's nested envelope `{"error": {"message": ...}}` with the cause
text of the error-code table; the body shape is an assumption until a
recording replaces it. The usage placement is also unknown: the schema nests
`prompt_cache_hit_tokens` and `prompt_cache_miss_tokens` under
`prompt_tokens_details`, the example has them at the top level. The two
transcripts carry one placement each; the decoder reads the cache hit only
from `cached_tokens`, so either placement counts it once.

A recorded smoke (`EGO_SMOKE=deepseek EGO_SMOKE_RECORD=1`) replaces these
files once an operator key is available.
