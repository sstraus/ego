# Public inventory recording

`models-2026-10-15.json` is the unmodified response recorded on 2026-10-15:

```sh
curl -fsS https://openrouter.ai/api/v1/models
```

No credential was sent. No generation was requested. The fixture includes the
public response, including unused price fields; ego does not import these prices.

The `supported_parameters` list is the authority for unified reasoning support.
Both `openai/gpt-6-luna` and `anthropic/claude-sonnet-5.5` publish `reasoning`.
The public reasoning guide, read on the same date, documents the normalized
`none`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max` effort values:
<https://openrouter.ai/docs/guides/best-practices/reasoning-tokens>.
The response also supplies per-model `reasoning.supported_efforts` and
`reasoning.default_effort`. Those exact lists, not the full vocabulary above,
determine the admitted efforts. Mandatory-reasoning models need not accept off.
A reasoning parameter without an effort list retains unknown effort support.
A model name is not capability evidence. An absent parameter grants no non-off
effort. With reasoning off, the OpenRouter mapping omits the effort field and sends
`reasoning: {"enabled": false}`. Omitting both would leave a reasoning model's
default-enabled setting active. The same public reasoning guide documents the
unified disable switch.

# Reasoning replay recording

`deepseek-reasoning-tool-call-2026-11-14.sse` is the unmodified streamed
response recorded on 2026-11-14 from `POST /api/v1/chat/completions` with model
`deepseek/deepseek-v4.1-flash-1017`, one `get_weather` tool, and the provider
pinned to the endpoint that served it (`Mistral`). The credential was read into
the recording process only and is not in the file. OpenRouter streams the text
as `delta.reasoning` beside `reasoning_details` items of type `reasoning.text`,
`format: "unknown"` and no signature, and it repeats `finish_reason` on the
usage chunk.

`deepseek-reasoning-continuation.request.json` is the second request of that
tool turn as the OpenRouter mapping builds it: the recorded reasoning goes back
as `reasoning_content` on the assistant message, beside its `tool_calls`.
The reasoning guide above documents `reasoning_content` as an alias of
`reasoning`, sufficient for models that return raw reasoning strings; signed or
encrypted reasoning needs `reasoning_details`, which this mapping does not send.
The OpenRouter row therefore admits replay only for the `deepseek/` family,
whose own published contract requires the earlier `reasoning_content` on tool
turns (<https://api-docs.deepseek.com/guides/thinking_mode/>). That is a
model-family contract, not a capability read from a name: no field of the
inventory distinguishes raw from signed reasoning.
