# Web search reference

Use [tools](../guides/tools.md) for the complete built-in tool overview and
[permissions](../guides/permissions.md) for authority and network boundaries.

## Provider-hosted web search

For an active `openai/...` or `anthropic/...` model, the enabled `web_search` tool uses that provider's hosted search contract and model credential. It does not need `EXA_API_KEY`. Use `--tools web_search` to select it, or `--no-tools` to disable tools. Hosted search can incur provider search fees and model token charges. Each generation allows at most five hosted searches. The currency budget reserves the search fee and a conservative allowance for repeated provider-side model inputs before sending the request.

Other providers keep the Exa search adapter and its existing explicit paid-search and network-permission checks. There is no silent fallback between native search and Exa. A provider or model that rejects native search returns that error. Provider-private search history cannot be silently replayed to another provider.

Search activity appears as reasoning progress. Cited sources appear as links in the assistant output, including the ACP assistant-content stream. Private provider content is kept in durable history for continuation but is excluded from public events and redacted exports. Anthropic `pause_turn` continues the provider's server-tool loop with the preserved content; it does not invoke a local tool.

The offline fixtures come from published provider examples. No live search conformance or per-model availability is claimed by these fixtures.
