# Providers and login

A provider profile selects an endpoint, credential strategy, wire contract,
capabilities, and metering. Changing a model does not change tool authority.
Use the catalogue to see what your current installation can actually select.

```sh
ego models --provider ollama --refresh
ego models --provider openai --json
```

The CLI inventory is defined by
[final_provider_rows](../../crates/ego-ai/src/conformance.rs) and
[provider.rs](../../crates/ego-ai/src/provider.rs). A row's presence is not proof
of a successful live login or entitlement to every model.

| Profile | Contract / model facts | Credential path |
| --- | --- | --- |
| `ollama` | Local daemon; discovered capabilities, local unmetered | No key; `OLLAMA_HOST` optional |
| `openai` | Public Responses; supplied catalogue | `OPENAI_API_KEY` |
| `anthropic` | Messages; supplied catalogue | `ANTHROPIC_API_KEY` or subscription credential |
| `gemini` | Interactions; supplied catalogue | `GEMINI_API_KEY` |
| `openrouter` | Its chat-completions gateway; operator-supplied model capabilities | `OPENROUTER_API_KEY` |
| `openai-codex` | ChatGPT/Codex subscription Responses; supplied bootstrap | `ego auth login openai-codex` |
| `github-copilot` | Copilot subscription completions/discovery | `ego auth login github-copilot` |
| `xai-oauth` | Distinct xAI subscription profile | `ego auth login xai-oauth` |
| `kimi-coding` | Kimi coding subscription | `ego auth login kimi-coding` |
| `minimax-oauth` | MiniMax subscription Messages profile | `ego auth login minimax-oauth` |
| `nous` | Nous subscription profile | `ego auth login nous` |

For `openrouter`, rows in the user `models.toml` supply the capabilities of
models that the gateway lists dynamically. The run and `ego models` both use
these rows. Rows for other providers do not replace their discovered or supplied
facts.

The separate API-key `xai` row is not in the CLI's selectable inventory. Design
mentions of LM Studio, vLLM, or llama.cpp are not additional selectable profiles.

## API keys and model selection

Set the relevant key in the process environment using your secret-management
workflow. Ego does not load a repository `.env` file. For Anthropic, a key
available as `ANTHROPIC_API_KEY` in ego's process environment works; a key only
in `.env` does not. The catalogue's `missing_credential` message recommends
`ego auth login anthropic` but does not mention this environment alternative.
Do not print the key or paste it into the conversation. Then select a listed full slug with `-m`. Current built-in defaults
come from [models.toml](../../crates/ego-cli/catalogue/models.toml), not a hard-coded
name in this guide. They currently name `openai/gpt-6.1-sol`,
`openai-codex/gpt-6.1-sol`, `anthropic/claude-opus-5-5`, and
`gemini/gemini-3.8-flash`. This describes repository policy, not external vendor
availability. `ego models` and generation-time resolution check credentials and
capabilities again.

OpenAI and Gemini are selectable; old research that required a live recording
before offering these public contracts was amended. Deterministic contract
conformance is not a claim that this documentation run called every hosted vendor.

## Codex transport

The `openai-codex` subscription mapping uses a run-owned WebSocket connection.
It sends `response.create` and requests `reasoning.encrypted_content`; opaque
reasoning can be replayed only through the matching provider dialect.
After a completed response, the next request can send `previous_response_id`
and only the new input items. Reuse requires the same active session, unchanged
non-input settings, and an exact prefix of the previous input plus normalized
completed output. A fork, changed prefix after compaction, unidentified request,
or fresh generator does not inherit another session's continuation.

HTTP SSE remains a fallback only when opening the connection fails with a
**definitely-unsent connect failure**. Once a request might have been sent,
transport failure is not permission to send it again over SSE. The socket and
each request remain under the run's admitted network authority.

Sources: [Codex mapping](../../crates/ego-ai/src/wire/openai_codex_responses.rs),
[continuation owner](../../crates/ego-ai/src/codex_session.rs),
[generator dispatch](../../crates/ego-ai/src/generator.rs), and
[guarded CLI transport](../../crates/ego-cli/src/provider_http.rs).
Existing [public boundary coverage](../../crates/ego-ai/tests/generator.rs)
and [CLI generation coverage](../../crates/ego-cli/src/model.rs) protect
same-session continuation and lineage invalidation. These are implementation
claims, not measured cache-hit parity or latency improvements. regression case's
live workload targets remain separate from the shipped transport.

## Subscription login

`ego auth login PROVIDER` runs the profile's built-in supported login flow or
configured credential program; `--device` requests its device flow when supported.
Follow the displayed browser/device instructions yourself. `ego auth status
[PROVIDER]` reports kind/expiry without printing tokens. `ego auth logout PROVIDER`
removes the stored credential.

The private store is `$EGO_HOME/auth.json`. Login, refresh, and logout use one
locked read-modify-write authority. A configured `auth.PROVIDER.credential_command`
can supply `login` and `refresh`; an `ego-auth-PROVIDER` helper is another supported
source. Missing flows fail with the required configuration instead of imitating
a vendor login.

Some subscription endpoints reject εgo's identity or require an entitlement.
A 401/403 is not proof of a different wire protocol. εgo does not ship another
client's identity. Non-secret operator header overrides are documented in
[configuration](../reference/configuration.md); credentials/wire headers remain
forbidden there. Do not put tokens in a project file, captured output, or issue.

## Capability and cost facts

Hosted model facts merge builtin, system, and user rows per slug. The bootstrap
contains no prices. A missing context, effort, image, or tool-limit fact stays
unknown and fails closed where needed. Image bytes enter session blobs before
provider-specific encoding; selecting a model without image support refuses them.

Subscription metering, local unmetered work, and known API pricing are different.
Unknown monetary cost does not become USD 0. A monetary ceiling requires a usable
quote; a token ceiling does not invent one. See
[configuration](../reference/configuration.md) for every model/pricing field.

Sources and existing coverage: [model selection](../../crates/ego-cli/src/model.rs),
[authentication](../../crates/ego-cli/src/auth.rs),
[provider conformance tests](../../crates/ego-ai/tests/profiles/mod.rs), and
[catalogue command tests](../../crates/ego-cli/tests/models_command.rs).

Provider function names are wire identities. Ego keeps the original tool name
in the catalogue and journal, and maps it at the provider boundary when it is
not a portable ASCII function name of at most 64 bytes. The same request-local
map encodes historical tool calls and restores streamed calls before dispatch.
Names in the reserved `__ego_` namespace are also mapped to prevent an original
name from impersonating an encoded name. Assignment is deterministic and checks
collisions across the complete request, including historical tool names.

Before installing a candidate binary for TUIC AI Chat, run
`python3 scripts/ai-chat-smoke.py /absolute/path/to/ego`. The gate starts the
candidate as `ego acp -C ~/Gits`, uses the real user profile, and relays the live
TUIC MCP catalogue over ACP. It checks a new turn and a resumed turn for each
available configured provider, starting with Codex subscription. Each turn must
have a provider HTTP 200 and a non-empty reply. `--models` supplies an explicit
model matrix when a configured provider needs login or a particular model must
be verified. Evidence stays under `~/Gits/.tmp`; the gate never installs ego.
