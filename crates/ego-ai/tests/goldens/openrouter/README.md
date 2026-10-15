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
