# Providers and login

Read this page in the [hosted user guide](https://sstraus.github.io/ego/guides/providers.html).

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
| `bedrock` | Claude in Amazon Bedrock, Messages; supplied catalogue | `AWS_BEARER_TOKEN_BEDROCK`; region from `AWS_REGION` |
| `vertex` | Claude Models on Google Cloud (Vertex AI); supplied catalogue | `GOOGLE_CLOUD_PROJECT` and `ego auth login vertex` |

For `openrouter`, rows in the user `models.toml` supply the capabilities of
models that the gateway lists dynamically. The run and `ego models` both use
these rows. Rows for other providers do not replace their discovered or supplied
facts.

The separate API-key `xai` row is not in the CLI's selectable inventory. Design
mentions of LM Studio, vLLM, or llama.cpp are not additional selectable profiles.

## API keys and model selection

`ego setup` lists every profile above as ready or not ready, with the variable or
login it still needs. It checks presence only and never shows a key.

Set the relevant key in the process environment using your secret-management
workflow. Ego does not load a repository `.env` file. For Anthropic, a key
available as `ANTHROPIC_API_KEY` in ego's process environment works; a key only
in `.env` does not. Do not print the key or paste it into the conversation. Then select a listed full slug with `-m`. Current built-in defaults
come from [models.toml](../../crates/ego-cli/catalogue/models.toml), not a hard-coded
name in this guide. They currently name `openai/gpt-6.1-sol`,
`openai-codex/gpt-6.1-sol`, `anthropic/claude-opus-5-5`, and
`gemini/gemini-3.8-flash`. This describes repository policy, not external vendor
availability. `ego models` and generation-time resolution check credentials and
capabilities again.

OpenAI and Gemini are selectable. Offline contract tests do not prove live
availability for every hosted provider.

## Amazon Bedrock and Google Cloud Vertex AI

Both rows serve Claude models through the cloud account of your company. The
request and the stream are the Claude Messages API; billing, quotas and data
handling are the cloud provider's.

### Amazon Bedrock

1. Enable access to the Claude models in the Amazon Bedrock console.
2. Create an Amazon Bedrock API key, or mint a short-term token from your IAM
   credentials with `aws-bedrock-token-generator`. Put it in
   `AWS_BEARER_TOKEN_BEDROCK`. Ego sends it in the `x-api-key` header.
3. Set `AWS_REGION` (or `AWS_DEFAULT_REGION`) to a region that serves Claude.
   Without one, ego uses `us-east-1`.
4. Select a model with its Bedrock id, for example
   `ego -m bedrock/anthropic.claude-opus-5-5`.

Ego sends requests to `https://bedrock-mantle.REGION.api.aws/anthropic/v1/messages`,
the endpoint for Claude Opus 4.7 and later models. The older `InvokeModel` and
`Converse` APIs are not used. Ego does not sign requests with IAM SigV4; use a
Bedrock API key or a minted token. A short-term token expires (12 hours at
most): mint a new one and set the variable again.

### Google Cloud Vertex AI

1. Enable the Claude models in Model Garden for your project. The identity that
   calls them needs the `aiplatform.endpoints.predict` permission, for example
   the role `roles/aiplatform.user`.
2. Create Application Default Credentials: run
   `gcloud auth application-default login`. To use a file in another place,
   set `GOOGLE_APPLICATION_CREDENTIALS` to it.
3. Set `GOOGLE_CLOUD_PROJECT` to the project ID. Set `GOOGLE_CLOUD_LOCATION` to
   `us`, `eu` or a region only when you need data residency; the default is
   `global`.
4. Run `ego auth login vertex`. Ego reads the credentials file, gets an access
   token from `oauth2.googleapis.com`, and keeps it in `$EGO_HOME/auth.json`.
   Before the token expires, ego gets a new one from the same file. If you
   replace or revoke the file, the next renewal uses the change.
5. Select a model, for example `ego -m vertex/claude-opus-5-5`.

Ego accepts only the `authorized_user` credential type, which
`gcloud auth application-default login` writes. Service-account key files
(`service_account`), workload identity federation (`external_account`) and the
metadata server of a Google Cloud VM are not supported; ego refuses the file
with its type name.
Claude 4.7 and later models are on the global and multi-region endpoints only;
a regional location serves older models.

### What differs from the Anthropic row

- The model list is the current Claude generation. A cloud-only or older model
  needs a row in your `models.toml`.
- Prompt caching uses the same 5-minute and 1-hour durations.
- Thinking state is kept per platform: a session that changes between
  `anthropic`, `bedrock` and `vertex` refuses to send thinking blocks from the
  other platform, because no published source says that they are accepted.
- Server-side web search and fast mode are not offered.
- Prices come from the litellm table (`bedrock_mantle` and
  `vertex_ai-anthropic_models` entries). The Bedrock entries are the regional
  prices. The Vertex entries are the global prices: a `us`, `eu` or regional
  location costs 10% more than the price ego records.
- The fixtures in `crates/ego-ai/tests/goldens/profile/bedrock` and `vertex` are
  derived from the published contract. To record them from the live service on
  a machine with access, run
  `EGO_SMOKE=bedrock EGO_SMOKE_RECORD=1 cargo test -p ego-cli --test it smoke -- --ignored`
  (or `EGO_SMOKE=vertex`).

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
Existing [public boundary coverage](../../crates/ego-ai/tests/it/generator.rs)
and [CLI generation coverage](../../crates/ego-cli/src/model.rs) protect
same-session continuation and lineage invalidation. These are implementation
claims, not measured cache-hit parity or latency improvements.

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
[configuration](../reference/configuration.md). An optional, user-owned
[protocol filter](../integrations/protocol-filter.md) can change final HTTP
generation bytes and declared identity headers; it is not a supported vendor
identity or a subscription guarantee. Credentials/wire headers remain forbidden
there. Do not put tokens in a project file, captured output, or issue.

## Explicit HTTPS proxy

`--proxy http://HOST[:PORT]` sends provider HTTPS requests through one HTTP
CONNECT proxy for this invocation. It applies to `ego run`, interactive `ego`, and
`ego resume`; specify it again when you resume. The URL must use `http://` with
no credentials, path, query, or fragment; the default port is 80.
`--proxy-ca FILE` adds one PEM CA certificate (at most 64 KiB) to this
invocation's provider TLS trust and requires `--proxy`. Certificate verification
stays enabled. A trusted CA lets that proxy read inference content and provider
credentials.

Ambient `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` values do not
select or bypass the proxy. A proxy failure does not fall back to a direct
connection. `--offline` refuses every proxy, including a loopback proxy. With
`--proxy`, HTTP provider endpoints and WebSocket inference are refused. The
proxy does not carry MCP, OAuth login, attachment downloads, served protocol
hosts, or standalone commands such as `ego models` and `ego compact`.

Sources: [option parsing](../../crates/ego-cli/src/cli/proxy.rs),
[proxy transport](../../crates/ego-tools/src/http/proxy.rs), and
[provider HTTP binding](../../crates/ego-cli/src/provider_http.rs).

## Capability and cost facts

Hosted model facts merge builtin, system, and user rows per slug. The bootstrap
contains no prices. A missing context, effort, image, or tool-limit fact stays
unknown and fails closed where needed. Image bytes enter session blobs before
provider-specific encoding; selecting a model without image support refuses them.

Subscription metering, local unmetered work, and known API pricing are different.
Unknown monetary cost does not become USD 0. Monetary admission checks recorded
response spend only and refuses a new request after that spend exceeds the cap.
There are no worst-case reservations. A sequential stream can overshoot by one
request; concurrent requests already in flight on the shared parent/child gate
may still settle after the cap is crossed. Equality still admits a request.

A durable cost-anomaly notice appears once per session/model, including after
resume. Unknown actual cost adds no invented estimate to spend. A later known
response cost still counts against the cap. Unpriced work is not covered by it.
Subscription and local metering remain distinct from unknown API prices;
a token ceiling does not invent a monetary quote. See
[configuration](../reference/configuration.md) for every model/pricing field.

Sources: [budget admission and settlement](../../crates/ego-core/src/budget.rs),
[durable notice deduplication](../../crates/ego-core/src/turn/journal.rs), and
[budget regression coverage](../../crates/ego-core/tests/it/budget.rs).

Sources and existing coverage: [model selection](../../crates/ego-cli/src/model.rs),
[authentication](../../crates/ego-cli/src/auth.rs),
[provider conformance tests](../../crates/ego-ai/tests/profiles/mod.rs), and
[catalogue command tests](../../crates/ego-cli/tests/it/models_command.rs).

Provider function names are wire identities. Ego keeps the original tool name
in the catalogue and journal, and maps it at the provider boundary when it is
not a portable ASCII function name of at most 64 bytes. The same request-local
map encodes historical tool calls and restores streamed calls before dispatch.
Names in the reserved `__ego_` namespace are also mapped to prevent an original
name from impersonating an encoded name. Assignment is deterministic and checks
collisions across the complete request, including historical tool names.

For a host integration smoke check, launch ego with `ego acp -C
/workspace/project` and use the host's supported ACP connection. Verify a new
turn and a resumed turn with a provider you have configured. Hosted checks can
incur charges and require credentials; they are not part of the offline gate.
