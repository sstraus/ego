# Synthetic usage-limit response

`openrouter-key.json` uses the schema observed in an authenticated HTTP 200
response to `GET https://openrouter.ai/api/v1/key`. All account values are
synthetic: the configured limit, remaining credit, lifetime usage and reset
schedule do not describe the observed account. The original capture clock,
credential source and whole-response fingerprint are omitted. No key, label,
account identity or authorization header is included.

The synthetic remaining value is not `limit - usage`: usage is a distinct
provider counter. Tests must retain that distinction and exact decimal
precision. Null, missing, negative and wrong-type cases are explicit mutations
of this synthetic example, not claims that those responses were observed live.

Header parser cases are adversarial lexical inputs. They are not recorded
Anthropic or OpenAI account responses. No paid call was made to obtain one.
