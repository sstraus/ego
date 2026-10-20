# Service tier endpoint fixtures

Recorded on 2026-10-18 with unauthenticated GET requests to the public OpenRouter
model endpoint listings below. No API key, credentials, or account identifiers were
sent. These requests do not generate model output.

Each fixture retains only the fields read by the service tier regression test:
`data.id`, the first `priority` endpoint and its matching standard endpoint,
`tag`, `service_tier`, and the `prompt`, `completion`, `input_cache_read`, and
`input_cache_write` pricing fields when present. Values are copied from the
response without changes. Other endpoints and metadata are omitted. The hashes
identify the full response bodies before this projection.

The test compares all seven canonical model slugs and the exact decimal token
rates. It does not require network access or the local evidence directory.

| Fixture | Source URL | Full response SHA-256 |
| --- | --- | --- |
| `anthropic-claude-opus-4-8-endpoints.json` | https://openrouter.ai/api/v1/models/anthropic/claude-opus-4.8/endpoints | `2c207a3976612422e12d353749487c041a68b2cbc9fe8f5ea631918cd0b2c079` |
| `anthropic-claude-opus-5-5-endpoints.json` | https://openrouter.ai/api/v1/models/anthropic/claude-opus-5.5/endpoints | `e133eecd2b5a6aad9f021d0db45b385007502eef062b1c027d53bda48015a47d` |
| `anthropic-claude-opus-5-endpoints.json` | https://openrouter.ai/api/v1/models/anthropic/claude-opus-5/endpoints | `c7d260a0424cb68432576768968801364c0e6e34169162f1567fe9594cebeac0` |
| `openai-gpt-6-astra-endpoints.json` | https://openrouter.ai/api/v1/models/openai/gpt-6-astra/endpoints | `3e483b90a5749f5b990d4e9fa1359e13537504c5da9eddce7a8dfa43b1f81e8f` |
| `openai-gpt-6-luna-endpoints.json` | https://openrouter.ai/api/v1/models/openai/gpt-6-luna/endpoints | `ddf64ca4b3c820b600b5b8c5a6f139bc98e7931d3c686cebc0751d76fb70dfac` |
| `openai-gpt-6-sol-endpoints.json` | https://openrouter.ai/api/v1/models/openai/gpt-6-sol/endpoints | `eb5568f82a7a33af20daeffcfd237e5ae626a260c2fe3e5110a0454b9a2e77b3` |
| `openai-gpt-6.1-sol-endpoints.json` | https://openrouter.ai/api/v1/models/openai/gpt-6.1-sol/endpoints | `b724402297a42d652fed2cc58670f35f659aa03bbebcddd65935a976ca85648a` |
