# Providers and login

Read this page in the [hosted user guide](https://ego.tuicommander.com/docs/guides/providers.html).

A provider profile selects an endpoint, credential strategy, wire contract,
capabilities, and metering. Changing a model does not change tool authority.
Use the catalogue to see what your current installation can actually select.

```sh
ego models --provider ollama --refresh
ego models --provider openai --json
```

## Start ego from a launcher or by hand

A launcher such as `ollama launch` starts ego as a child process with the
terminal inherited. You can start it the same way yourself:

```sh
OLLAMA_HOST=127.0.0.1:11434 ego -m ollama/qwen3:8b
```

- Model: `-m ollama/<model>`. Only the first `/` ends the provider name, so
  `ollama/<namespace>/<name>:<tag>` selects the model `<namespace>/<name>:<tag>`.
  `EGO_MODEL` sets the same choice. A `:cloud` model is refused, because the
  daemon relays it to a hosted service and a loopback endpoint does not prove
  local, unmetered execution.
- Daemon address: `OLLAMA_HOST`. An address that names the bind address
  (`0.0.0.0` or `[::]`, with or without scheme and port) means the local
  machine: ego connects to `127.0.0.1` or `[::1]`. Other hosts stay as given.
- Config isolation: set `EGO_HOME` to a separate directory to keep the
  launcher's configuration, credentials, and sessions apart from your own.
- Workspace trust: ego asks for it interactively on first use of a workspace.
  Answer the question. Do not add `--trust-workspace` to a launcher command
  unless you accept it for every workspace the launcher opens.
- Exit: Ctrl+C cancels a running turn; the process exit code is returned to the
  launcher unchanged.

The CLI inventory is defined by
[final_provider_rows](../../crates/ego-ai/src/conformance.rs) and
[provider.rs](../../crates/ego-ai/src/provider.rs). A row's presence is not proof
of a successful live login or entitlement to every model. The inventory has 19
profiles:

| Profile | Contract / model facts | Credential path |
| --- | --- | --- |
| `ollama` | Local daemon; discovered capabilities, local unmetered | No key; `OLLAMA_HOST` optional |
| `openai` | Public Responses; supplied catalogue | `OPENAI_API_KEY`; `OPENAI_BASE_URL` optional |
| `anthropic` | Messages; supplied catalogue | `ANTHROPIC_API_KEY` or subscription credential; `ANTHROPIC_BASE_URL` optional |
| `gemini` | Interactions; supplied catalogue | `GEMINI_API_KEY` |
| `openrouter` | Its chat-completions gateway; operator-supplied model capabilities | `OPENROUTER_API_KEY`; `OPENROUTER_BASE_URL` optional |
| `openai-codex` | subscription Responses; supplied bootstrap | `ego auth login openai-codex` |
| `github-copilot` | subscription completions/discovery | `ego auth login github-copilot` |
| `xai-oauth` | Distinct xAI subscription profile | `ego auth login xai-oauth` |
| `kimi-coding` | Kimi coding subscription | `ego auth login kimi-coding` |
| `minimax-oauth` | MiniMax subscription Messages profile | `ego auth login minimax-oauth` |
| `nous` | Nous subscription profile | `ego auth login nous` |
| `bedrock` | Anthropic models in Amazon Bedrock, Messages; supplied catalogue | `AWS_BEARER_TOKEN_BEDROCK`; region from `AWS_REGION` |
| `vertex` | Anthropic models on Google Cloud (Vertex AI); supplied catalogue | `GOOGLE_CLOUD_PROJECT` and `ego auth login vertex` |
| `vertex-gemini` | Google Cloud Gemini Interactions through a configured instance | Named OAuth connection from `ego auth login vertex-gemini` |
| `deepseek` | DeepSeek Chat Completions; supplied catalogue | `DEEPSEEK_API_KEY` |
| `bedrock-runtime` | Bedrock Runtime (Converse or selected Chat); configured instances only | Named bearer key or SigV4 credential document; region from the instance's `base_url` |
| `zai` | Z.ai API (GLM), separate from its coding subscription; no built-in models | `ZAI_API_KEY` |
| `kimi-api` | Moonshot Kimi API, separate from `kimi-coding`; no built-in models | `MOONSHOT_API_KEY` |
| `mistral` | Mistral Conversations; no built-in models | `MISTRAL_API_KEY` |

`OLLAMA_HOST` and `OPENAI_BASE_URL` name a server, and ego uses its `/v1/`
path. `ANTHROPIC_BASE_URL` keeps a gateway path prefix and appends `/v1/` only
when it is absent. `OPENROUTER_BASE_URL` replaces the complete OpenRouter
API base, including its version path (default `https://openrouter.ai/api/v1/`).
Profiles without built-in models need the model rows described in
[additional configured contracts](#additional-exact-configured-contracts).

For `openrouter`, rows in the user `models.toml` supply the capabilities of
models that the gateway lists dynamically. The run and `ego models` both use
these rows. Rows for other providers do not replace their discovered or supplied
facts.

DeepSeek models (`openrouter/deepseek/...`) keep their reasoning across tool
calls: ego sends the reasoning of each earlier assistant turn back as
`reasoning_content` on requests that offer tools, including when it reloads
the saved session from disk. A stream interrupted before the tool-call finish
does not save partial reasoning for replay. Other
routed models replay no reasoning, because their reasoning is signed or
encrypted and needs a field ego does not send. A switch to a model of another
provider drops the replayed reasoning and keeps the visible text, tool calls,
and tool results. Direct DeepSeek and OpenRouter DeepSeek use distinct replay
dialects even though both send Chat Completions requests.

There is no API-key `xai` profile; only the `xai-oauth` subscription profile
exists. For LM Studio, vLLM and llama.cpp, see
[operator-configured gateways and local servers](#operator-configured-gateways-and-local-servers).

## API keys and model selection

`ego setup` lists every profile above as ready or not ready, with the variable or
login it still needs. It checks presence only and never shows a key.

For a provider that accepts an API key, such as Gemini, run:

```sh
ego auth login gemini
```

Wait for the hidden prompt, then enter the key there, not in the command or in
chat. Ego asks even if the environment already contains a key. It saves the
entered key in its private credential store, not in TOML. Key entry requires an
interactive terminal; pipes and redirected input are not supported.

For an OAuth provider, such as OpenAI Codex, run:

```sh
ego auth login openai-codex
```

Follow the authorization instructions that ego displays. There is no API key to
enter. Neither example needs `--method`: ego uses the only available method.
If both API-key and OAuth login are enabled for a provider, ego asks you to
choose in the terminal. `--method api-key` and `--method oauth` are optional
shortcuts to skip that choice; a noninteractive OAuth login on a dual-method
route needs `--method oauth` or `--device` to select OAuth.

Both examples register a separate account without changing your default model
or a configured account default. For subsequent commands, use `--account N`
if the provider has several accounts.
If you ran login in another terminal, an already-open session keeps its
original account binding. In `/model`, a saved key outside that binding is
shown with instructions to open a new session using the saved connection.

Alternatively, set the relevant key in the process environment using your secret-management
workflow. Ego does not load a repository `.env` file. For Anthropic, a key
available as `ANTHROPIC_API_KEY` in ego's process environment works; a key only
in `.env` does not. Do not print the key or paste it into the conversation. Then select a listed full slug with `-m`. Current built-in defaults
come from [models.toml](../../crates/ego-cli/catalogue/models.toml), not a hard-coded
name in this guide. They currently name `openai/gpt-6.1-sol`,
`openai-codex/gpt-6.1-sol`, `anthropic/claude-opus-5-5`, and
`gemini/gemini-3.8-flash`. This describes repository policy, not external vendor
availability. `ego models` and generation-time resolution check credentials and
capabilities again.

The rows follow each vendor's published contract. Offline conformance tests
check the request and response shapes; they do not show that every hosted
vendor accepts every account.

## Multiple accounts

A provider can have several saved OAuth or key-backed accounts. If only one
applies, omit the selector. Otherwise choose the account number shown by
`ego auth status PROVIDER`. A number belongs to the provider, not to the row
in today's list. Names can be changed without affecting account selection.

```sh
ego auth status gemini
ego models --provider gemini --account 2
ego --account 2 -m gemini/gemini-3.8-flash
ego auth login gemini --renew --account 2
ego auth remove gemini --account 1
```

After removal, account 2 stays account 2 and, if it is now the only account,
no selector is needed. A later login allocates account 3 rather than reusing 1.
`logout` removes the stored credential but keeps the account and its number;
`remove` deletes the saved account and any stored credential. For an
environment-backed account, `logout` refuses; unset the declared variable to
withdraw that key. Neither command revokes a key at the provider.

A plain `ego auth login PROVIDER` creates a separate account. Use `--renew`
for the same upstream account when rotating a credential; select `--account N`
if the provider has several accounts. Environment-backed accounts use only their
declared variable and do not accept stored-key renewal. See the
[configuration reference](../reference/configuration.md#named-account-connections).

An open terminal session keeps its admitted account. If `/model` offers another
account, it gives you a command for a **new session**, not a way to transfer the
current session's private history. Plain resume, fork and reconnect preserve
the saved account or refuse an unavailable binding. An explicit CLI resume
model from another provider resolves that provider’s account; add `--account N`
if ambiguous. Provider-private history is still bound to the account that made it.

To keep two OAuth accounts, sign in to each with a separate `ego auth login
PROVIDER`. Find both numbers with `ego auth status PROVIDER`. Start a session
with account 2 using `ego --account 2 -m PROVIDER/MODEL`. To rename it, change
only `name` in its `[connections.ID]` table. `ego resume SESSION` still uses
account 2, even if account 1 is the default or you sign it out with
`ego auth logout PROVIDER --account 1`.

Existing named accounts without numbers are numbered when processed by
`ego auth status`. Older provider-keyed credentials are ignored and left
untouched. There is no conversion command. Sign in again to create a named
account; this does not transfer private history from an unbound session.

### Account status and quota

`ego auth status --json` prints which credentials are present, without secrets,
labels or expiry:

```json
{"v":1,"credentials":[{"provider":"anthropic","connection":"account_a","present":true},{"connection":"orphan_a","present":true}]}
```

Every record identifies a named `connection`. For configured accounts, `provider`
is the configured vendor. An unfiltered inventory can also show a stored orphan
whose account metadata is missing; that record has no `provider`. Provider-wide
legacy slots are not included or used as account fallbacks.

Presence means only that the local source exists: a nonblank declared environment
variable for an environment-backed key account, a saved API key for a stored-key
account, or a stored entry for an OAuth account. It does not check validity,
expiry or entitlement and contacts no provider.

`ego auth usage [PROVIDER] [--json]` reads the remaining subscription quota of
stored OAuth credentials from the provider's usage endpoint, without generating
a response, refreshing a token or signing in. Use `--account N` with a provider to read one saved account. The JSON has `rate_limits` windows and `errors` per provider
(`auth_invalid`, `backoff_until` or `unavailable`); key connections report
`unavailable`. Processes that share one `EGO_HOME` read at most once every 300
seconds per credential.

Provider-hosted `web_search` uses the selected generation connection on an
admitted endpoint/model route; `--tools web_search` never substitutes the
independent `web_search_external` recipient. Search availability, entitlement,
bounds and billing remain route-specific. See the [web search reference](../reference/tools.md)
for admission, explicit selection and consent. A saved connection does not
make search available on a route that lacks it.

Named-account subscription image generation remains unimplemented. Such runs
do not borrow the provider-wide subscription image credential; an explicitly
configured API image backend remains separately paid and consent-gated. See
[image generation](tools.md).

## Amazon Bedrock and Google Cloud Vertex AI

Both rows serve Anthropic models through the cloud account of your company. The
request and the stream are the Anthropic Messages API; billing, quotas and data
handling are the cloud provider's.

### Amazon Bedrock

1. Enable access to the Anthropic models in the Amazon Bedrock console.
2. Create an Amazon Bedrock API key, or mint a short-term token from your IAM
   credentials with `aws-bedrock-token-generator`. Put it in
   `AWS_BEARER_TOKEN_BEDROCK`. Ego sends it in the `x-api-key` header.
3. Set `AWS_REGION` (or `AWS_DEFAULT_REGION`) to a region that serves these models.
   Without one, ego uses `us-east-1`.
4. Select a model with its Bedrock id, for example
   `ego -m bedrock/anthropic.claude-opus-5-5`.

Ego sends requests to `https://bedrock-mantle.REGION.api.aws/anthropic/v1/messages`,
the endpoint for Opus 4.7 and later models. The older `InvokeModel` and
`Converse` APIs are not used. Ego does not sign requests with IAM SigV4; use a
Bedrock API key or a minted token. A short-term token expires (12 hours at
most): mint a new one and set the variable again.

### Google Cloud Vertex AI

1. Enable the Anthropic models in Model Garden for your project. The identity that
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

Login creates a named OAuth connection. Requests use only that connection's
stored token, not a legacy provider token or an ambient access token. The
project and location remain the declared environment inputs above; they are
request context, not alternate credential sources. Login refuses a missing
project or an invalid project/location before it publishes the connection.

Ego accepts only the `authorized_user` credential type, which
`gcloud auth application-default login` writes. Service-account key files
(`service_account`), workload identity federation (`external_account`) and the
metadata server of a Google Cloud VM are not supported; ego refuses the file
with its type name.
Anthropic 4.7 and later models are on the global and multi-region endpoints only;
a regional location serves older models.

### Gemini on Google Cloud Vertex AI

> **Unverified route.** No exchange with the live Vertex AI service has been
> recorded. Offline tests cover the request URL, authentication and headers,
> but the streamed usage and the thought continuation after a tool result
> come from the published API schema only.

Run `gcloud auth application-default login` and then
`ego auth login vertex-gemini`. Use the saved connection ID as the instance's
`credential_ref`. The connection must declare `vendor = "vertex-gemini"` and
`access = "oauth"`. The base URL must include the project and matching location.

| Location | Instance `base_url` |
| --- | --- |
| `global` | `https://aiplatform.googleapis.com/v1/projects/PROJECT_ID/locations/global/` |
| `us` | `https://aiplatform.us.rep.googleapis.com/v1/projects/PROJECT_ID/locations/us/` |
| `eu` | `https://aiplatform.eu.rep.googleapis.com/v1/projects/PROJECT_ID/locations/eu/` |

Declare `gemini-4-argon` as the upstream ID with the `gemini-interactions`
contract. Use SSE, an explicit tool limit, context up to 1,048,576 tokens and
foreground output up to 65,536 tokens. Specify Low, Medium and High thinking,
default Medium, and same-provider replay with the `gemini-interactions`
dialect. Set cache retention to `none` and audio to `unsupported`. Supply the
model's metering identity explicitly.

The instance URL determines project and location. The built-in ADC login
binds the connection to the ADC OAuth client and Google's stable account
identifier. Refresh reads the current ADC file, verifies the Google account
through OpenID UserInfo, and refuses a different identity before replacing the
saved token. Use the default gcloud OpenID scopes. To switch between accounts,
set `GOOGLE_APPLICATION_CREDENTIALS` to the intended file and select its
connection. If a saved ADC login predates principal binding, select the intended
file and run `ego auth --connection CONNECTION_ID login` once.

A credential command configured under `auth.vertex-gemini` supplies both login
and refresh when present and is responsible for keeping its own account
identity stable. Set generation headers such as
`x-goog-user-project` in `[auth.vertex-gemini.headers]`. Follow the [Google Interactions guide](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/capabilities/interactions/developer-guide)
for project enablement and access.

### What differs from the Anthropic row

- The model list is the current Anthropic model generation. A cloud-only or older model
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

## DeepSeek

The `deepseek` row sends requests to `https://api.deepseek.com/chat/completions`
with the key in `DEEPSEEK_API_KEY` as a bearer token. Without the key, ego refuses
the run before it opens a connection and names the variable.

- Models: `deepseek/deepseek-flash` and `deepseek/deepseek-v4-pro`, with a 1M
  context and at most 393216 output tokens. The retired names
  `deepseek-v4-flash` and `deepseek-v4-flash-vision-exp` are not listed.
- Thinking: `--thinking off`, `low`, `high` or `max`; the default is `high`.
  `off` turns thinking off with DeepSeek's switch. Ego refuses `minimal`,
  `medium` and `xhigh` before the request, because DeepSeek would silently
  replace them with another level.
- Reasoning: on a request that offers tools, ego sends the exact
  `reasoning_content` of every earlier assistant turn back, as DeepSeek requires.
  Optional `reasoning` display extensions remain visible but are not durable
  DeepSeek replay state. A switch to a model of another provider drops it.
- Explicit hosted Web Search is available on requests without local function
  tools. Images, audio, parallel tool calls and other hosted tools are
  unsupported on this row.
- Errors: `401` is a refused key and `402` an empty balance; ego does not retry
  either. `429` and `503` are retryable.
- Price: DeepSeek's price changes with the time of day, and ego has no
  time-of-day tariff. The builtin catalogue and the row state no price, and
  the usage that DeepSeek returns has token counts only. To price DeepSeek,
  give your own rates in `models.toml`. With a money ceiling and no own rates,
  ego does not use the litellm table for this row: its one rate per model
  (DeepSeek's peak price on 2026-11-14) does not state the actual tariff. The
  cost stays unknown and ego reports it.
- The fixtures in `crates/ego-ai/tests/goldens/profile/deepseek` come from the
  published contract. To record them from the live service with a key, run
  `EGO_SMOKE=deepseek EGO_SMOKE_RECORD=1 cargo test -p ego-cli --test it smoke -- --ignored`.

## Trusted configured instances

Trusted user/system configuration can bind separate deployment IDs to exact
compiled profiles, endpoints, credential references, upstream aliases and model
facts. See the [configuration reference](../reference/configuration.md) for settings.
An endpoint's compatibility label or model listing is not capability evidence;
unsupported contracts and missing credential references refuse rather than
selecting a different account.

Model listing, startup/agent catalogues and setup readiness use the same validated
instance facts. Setup checks the named connection's declared environment source
or its saved credential of the matching kind. Readiness reports local presence.
Check entitlement and request success during generation. An admitted MCP
deployment and an existing instance-capable ACP session keep their captured
definitions when saved configuration changes. New sessions and settled sessions
reopened in a fresh host admit current configuration. Ordinary named-account
execution retains its existing identity rules.

The explicit `chat-max-tokens` mapping is available only under
`openai-compatible`, separately from `chat-standard`'s unchanged
`max_completion_tokens` mapping. Its bounded subset is nonreasoning text/tools
with Off-only/default-Off reasoning, an explicit operator tool limit, no enabled
parallel control or opaque state, SSE and no declared cache retention or positive
image/audio support. It preserves historical tool exchanges and omits unproven
parallel/stream-usage controls. False parallel capability does not promise one
server-generated call. Do not use the mapping alone to advertise complete
local-server/gateway support or to turn off a model that reasons by default.

### Azure OpenAI v1 deployments

> **Unverified route.** No exchange with a live Azure OpenAI resource has been
> recorded. Offline tests cover deployment resolution and the API-key and
> Entra credential paths, but the exact tool turn, usage and authentication
> errors come from the published API schema only.

Set the instance profile and named connection vendor to `azure-openai`. The
instance's `base_url` and the connection's `azure_resource` must name the
same HTTPS base, either `https://RESOURCE.openai.azure.com/openai/v1/` or
`https://RESOURCE.services.ai.azure.com/openai/v1/`. Set `credential_ref`
to that connection's ID. The local model alias can be arbitrary. Set each
model's `upstream_id` to the Azure deployment name and declare `responses`
or `chat-standard` with the deployment's known facts and metering. The
request uses the deployment name in the body and the v1 route without an
`api-version` parameter.

For keys, use `access = "key"` with a resource-specific `key_env`. Ego
presents the value as a bearer API key. For Entra, use `access = "oauth"`
and configure `[auth.azure-openai]` with a `credential_command` whose first
argument is the absolute path to your trusted helper. Ego appends
`--resource BASE --scope https://ai.azure.com/.default` before the final
`login` or `refresh` argument. Leave `--resource`, `--scope` and the
`--` option terminator out of the configured command so the helper receives
the admitted values. The helper must request that Entra scope and return the
existing OAuth JSON shape with an access token and a future `expires` value
in Unix milliseconds. It can include refresh state.
Ego passes prior OAuth state on refresh stdin. List account numbers with
`ego auth status azure-openai` and authenticate with
`ego auth login azure-openai --account N`. Renew the account when its resource
or helper changes. Keep secrets out of command arguments and config files.

This profile covers public Azure OpenAI-compatible v1 deployments, including
Foundry models whose exact deployed contract matches the selected mapping.
Distinct model-inference, project, sovereign-cloud and private gateway
protocols require their own contracts. Azure determines access, entitlement
and model support. Deployment discovery and built-in pricing are unavailable.

See the [Microsoft Azure OpenAI v1 contract](https://learn.microsoft.com/en-us/azure/foundry/openai/api-version-lifecycle) and
[Foundry endpoint contracts](https://learn.microsoft.com/en-us/azure/foundry/foundry-models/concepts/endpoints).

### Additional exact configured contracts

`bedrock-runtime` / `bedrock-converse` is separate from the existing Bedrock
Messages row. It uses a regional runtime base and API-key bearer presentation,
native `amazon_event_stream` framing for tool-free turns, and synchronous
Converse JSON for tool turns. Explicit Off-only/default-Off, text/tool limits
and no replay/cache/modality facts are required. A false parallel fact is not a server cardinality promise.

For Mistral Large 3 on the `us-east-1` runtime, set the model contract to
`bedrock-chat`, `upstream_id = "mistral.mistral-large-3-675b-instruct"`, and
`base_url = "https://bedrock-runtime.us-east-1.amazonaws.com/"` on the instance.
It uses the runtime Chat Completions path, SSE, and the bounded text/tool Chat
request shape described above. [AWS Mistral Large 3 model card](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-mistral-ai-mistral-large-3.html)

Select `profile = "bedrock-runtime"` for an API key or
`profile = "bedrock-runtime-sigv4"` for AWS signing. Set the named
connection’s `vendor` to the chosen profile and use `access = "key"`.
For SigV4, set `key_env` to a secret-marked variable such as
`TEAM_BEDROCK_CREDENTIALS_SECRET` and populate it with the version 1 AWS
credential-process JSON. It contains `AccessKeyId`, `SecretAccessKey`,
and, for temporary credentials, `SessionToken` and `Expiration`.
Select the AWS regional runtime root as the base. Renew expiring credentials
before starting a new run. Ego withholds configured SigV4 `key_env` variables
from built-in `exec` children, including when an exec allowlist names them.
SigV4 refuses redirects and protocol filters.
[External process credentials](https://docs.aws.amazon.com/sdkref/latest/guide/feature-process-credentials.html)

Runtime discovery, default models, prices and account entitlement are not
inferred. Check the region and scope of the credential against the model and
deployment you configure.

`zai` / `zai` and `kimi-api` / `kimi-api` bind the exact GLM-5.3 and Kimi K3.1
API-key text/tool mappings. Default roots are `https://api.z.ai/api/paas/v4/`
and `https://api.moonshot.ai/v1/`; ordinary key sources are `ZAI_API_KEY` and
`MOONSHOT_API_KEY`. Configured instances require matching explicit key connections.
Declare Low/High/Max efforts, default Max, SSE, explicit context/output/tool
limits, and provider-only replay with the exact `zai-glm53-chat` or
`kimi-api-k3-chat` dialect. Z.ai uses `max_tokens` and preserved thinking;
Kimi uses `max_completion_tokens`. Disabled cache bookkeeping is accepted.
Z.ai supports explicit hosted Web Search. Its SSE mapping omits search sources
and search counts. Kimi Chat does not support hosted search.
Both mappings refuse explicit cache controls, parallel controls, and non-text
input.
The API profiles do not inherit subscription credentials or invent default
models, discovery, prices or account entitlement. These bounded schema-backed
mappings are not live service certification.

`kimi-api` / `kimi-responses` uses Moonshot's `/v1/responses` route and the
`kimi-responses` replay dialect. It supports explicit hosted search. Use the
same Moonshot API-key connection requirements as Kimi Chat.

`mistral` / `mistral-conversations` uses
`https://api.mistral.ai/v1/conversations` with a bearer key from
`MISTRAL_API_KEY` or, for an instance, an explicit Mistral key connection.
It supports stateless replay and hosted search alongside client function tools.
Declare SSE, same-provider replay with the `mistral-conversations` dialect,
and explicit limits. Excluded controls include explicit cache retention,
parallel tool-call control and image or audio input. The request and stream
fixtures are schema-derived, and the mapping has no built-in model or tariff.
Mistral Chat Completions remains outside this contract. [Mistral Conversations API](https://docs.mistral.ai/api/endpoint/beta/conversations)

### Operator-configured gateways and local servers

Use an `openai-compatible` instance only for a deployment you can describe with
an admitted contract. The following base URLs identify candidate routes. The
configured alias, upstream model ID, context/output limits, tool support and
metering come from the specific deployment.

| Deployment | Base URL and credential | Contract boundary |
| --- | --- | --- |
| LM Studio | Usually `http://127.0.0.1:1234/v1/`; optional server token | Use the loaded model ID. Tool support depends on the model and template. [Chat Completions](https://lmstudio.ai/docs/developer/openai-compat/chat-completions), [authentication](https://lmstudio.ai/docs/developer/core/authentication) |
| vLLM | Usually `http://127.0.0.1:8000/v1/`; optional server API key | The served name, chat template and configured tool parser determine the deployment contract. [Serving API](https://docs.vllm.ai/en/latest/serving/online_serving/openai_compatible_server/) |
| llama.cpp | Usually `http://127.0.0.1:8080/v1/`; optional server API key | Check the server's served model ID, chat template and tool flags. [Server documentation](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md) |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh/v1/`; gateway bearer key | Use the gateway's namespaced model ID and its applicable tariff. [Gateway Chat Completions](https://vercel.com/docs/ai-gateway/sdks-and-apis/openai-chat-completions) |
| LiteLLM | Operator's proxy base; proxy or virtual key | Use the configured proxy alias and the proxy's effective wire and billing contract. [Proxy client setup](https://docs.litellm.ai/docs/proxy/user_keys) |

Set `credential_ref` to a named key connection with `vendor = "openai"` and a
dedicated key source whenever the server requires bearer authentication. Only an
exact-loopback `openai-compatible` instance may omit the reference. Select
`chat-max-tokens` only for a deployment that accepts its bounded nonreasoning
contract. A loopback proxy can forward to a paid provider, so declare unmetered
use only when the deployment actually has no per-use charge.

### Provider-specific exclusions

Groq, Together AI, Fireworks, Cerebras, MiniMax API, Alibaba/Qwen, Hugging
Face Inference and Cloudflare AI Gateway have no reviewed built-in profile or
tariff here. A published endpoint or a model listing alone cannot establish the
supported request controls or streamed tool continuation. MiniMax API keys
belong to a separate credential identity from the `minimax-oauth` subscription.

Perplexity's Agent API is a Responses route. Ego needs a current Responses
recording before adding it and has no verified external function-tool capability
for that service. [Perplexity Agent API compatibility](https://docs.perplexity.ai/docs/agent-api/openai-compatibility)

## Subscription transport

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

Sources: the subscription wire mapping,
the continuation owner,
[generator dispatch](../../crates/ego-ai/src/generator.rs), and
[guarded CLI transport](../../crates/ego-cli/src/provider_http.rs).
Existing public boundary coverage
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
generation bytes and request headers, including a replacement credential;
it is not a supported vendor identity or a subscription guarantee. A
replacement credential is a public header value in Ego. Do not put tokens in a project file, captured output, or issue.

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

### Fast service tier

Some models offer a faster tier at a higher token price. Set a per-model
default in the `[service_tier]` table of the user or system `models.toml`
(`$EGO_HOME/models.toml` or `/etc/ego/models.toml`):

```toml
[service_tier]
"openai/gpt-6.1-sol" = "fast"
"openai-codex/gpt-6.1-sol" = "standard"
```

`standard` is the default. `fast` is refused for a model unless the catalogue
has a verified fast tier and price for that full slug; `ego models` lists the
models with fast support and their price multiplier. In a session,
`/fast on` and `/fast off` change the tier of the current model for that
session; the receipt names the price premium. Cost estimates use the selected
tier, and a missing price stays unknown.

Sources: [budget admission and settlement](../../crates/ego-core/src/budget.rs),
[durable notice deduplication](../../crates/ego-core/src/turn/journal.rs), and
budget regression coverage.

Sources and existing coverage: [model selection](../../crates/ego-cli/src/model.rs),
[authentication](../../crates/ego-cli/src/auth.rs),
provider conformance tests, and
catalogue command tests.

Provider function names are wire identities. Ego keeps the original tool name
in the catalogue and journal, and maps it at the provider boundary when it is
not a portable ASCII function name of at most 64 bytes. The same request-local
map encodes historical tool calls and restores streamed calls before dispatch.
Names in the reserved `__ego_` namespace are also mapped to prevent an original
name from impersonating an encoded name. Assignment is deterministic and checks
collisions across the complete request, including historical tool names.
