# Provider protocol filter

A `protocol_filter` is an optional, user-owned executable that changes the outgoing HTTP
body and incoming streamed bytes of one provider generation attempt. It can also return
request headers, including replacements for existing credential headers. Ego negotiates the filter's interest before sending request bytes or dispatching
HTTP. One persistent process handles only the directions it declares. When the
filter declines a direction, Ego passes those bytes through without sending
them to the executable. Filtered response bytes reach the provider decoder
after the process transforms them. It does not act as an HTTPS proxy, general
plugin system, or provider compatibility guarantee.

This is a **provider transport hook** with outgoing-request and streamed-response
phases. Content filtering and reversible DLP are policies implemented by the
selected executable; its configuration key remains `protocol_filter`.
The supported filter is a local CLI executable, not an HTTP or MCP handler.
The same selected executable also checks outgoing HTTP MCP tool calls and
HTTP / `mcp_tool` lifecycle-hook payloads through a request-only exchange.

Use this feature only with an executable that you have reviewed. The executable
runs with your OS account. Ego clears its inherited environment, but that does
not prevent the executable from reading your files or opening its own sockets.
The request body can contain private conversation content.

## When to use a protocol filter

Use this boundary when a policy needs the **encoded provider request body** or
**raw provider response bytes**, rather than only an Ego prompt or final
answer. Select an executable for one provider in your user configuration.
It runs on each actual generation attempt, including retries. Test it with a
controlled provider fixture before using it with live data.

| Use case | What the executable can do | Main limit |
| --- | --- | --- |
| Reversible data masking | Replace selected private values before dispatch, save a local mapping, then restore values across streamed text, tool arguments and error responses. The [DLP example](#reversible-dlp-example) implements this policy. | Heuristic recognition can miss values. Restored values can reach local tools and the journal. Response restoration must remain enabled. |
| Request-body conformance check | Refuse a request before dispatch when its encoded body violates a rule, such as a forbidden body field or an unsupported provider control value. | The executable sees neither authentication headers nor the URL. This is not a replacement for Ego permissions or provider validation. |
| Provider-specific body adaptation | Change a known body field or content block to match a tested provider contract, then let Ego send the exact returned bytes. | A wrong change can invalidate the request or alter caching. Pin and retest the policy against the provider version; do not treat experimental request identity as a provider guarantee. |
| Incoming content sanitation | Replace a known sensitive pattern in streamed provider text or tool-argument fragments before local decoding, without restoring outbound placeholders. | The policy must retain split fragments. It cannot withdraw bytes already published, and it must handle error responses as well as successful streams. |
| Controlled request-identity experiment | Change a header or body fingerprint for a bounded comparison of two request shapes. | This is version-specific research, not authentication, entitlement or a supported provider identity. A replaced credential is the filter author's responsibility. |

Choose the reversible policy only if the response must restore masked data.
A request-only conformance or body-adaptation policy can declare interest in
requests only. Ego then passes response bytes through without a Python
exchange. A response-only policy can decline request bytes. A reversible DLP
policy must declare both directions. No direction can be skipped after the
executable has declared it for this attempt.

This hook cannot route to another host or read Ego's OAuth tokens. It cannot
bypass tool permissions or guarantee complete output inspection after
streamed bytes have already reached a consumer. It is not a general HTTP proxy.

## Install and select one executable

1. Place one executable at a stable absolute path and make it executable. A
   script needs a shebang for an interpreter available on the machine. On Unix,
   Ego supplies only `/usr/bin:/bin` as the child `PATH`; use an absolute
   interpreter path if your interpreter is elsewhere.
2. Edit the **user** `$EGO_HOME/config.toml` (by default
   `~/.ego/config.toml`). Replace `example` with an actual provider ID and the
   example path with your installed executable. Project, local, system, and
   invocation layers cannot declare `protocol_filter`.

   ```toml
   # Default for every provider unless overridden below.
   [protocol_filter]
   command = "/absolute/path/to/default-filter"
   timeout_ms = 10000

   [auth.example.protocol_filter]
   command = "/absolute/path/to/my-filter"
   timeout_ms = 10000
   enabled = true
   ```

3. Start a new session to load the saved configuration. To turn the filter off,
   remove the table or set `enabled = false`, then start a new session. No
   shipped provider default or model selection changes when you configure it.
   The managed `ego config set` key list does not include this table; use
   `ego config edit` or edit the user file and validate the complete TOML.

`command` is one absolute executable path, not a shell command. Ego supplies no
arguments or working-directory override. `timeout_ms` is 1 through 30000,
with a default of 10000. The limit applies to each child exchange; the
separate generation deadline and cancellation still apply. A stalled filter
can delay each exchange until its limit. `enabled`
defaults to true. A complete provider filter entry is selected from the user
layer; executable fields are not merged with other layers. The root
`[protocol_filter]` table is a user-only fallback. A provider table replaces
that whole fallback, never composes with it. `enabled = false` on a provider
entry opts that provider out even when the global fallback is enabled. A
disabled entry still requires its absolute `command` path. One process runs at
a time; filter chains are not supported. A command timeout before durable state
acceptance or external dispatch gets up to three retries with fresh processes
(four starts in total). Each restart uses the same generation and attempt IDs
and reads the current journal snapshot. The run deadline and cancellation still
apply. A timeout after state acceptance or during the response stream is terminal;
Ego does not replay accepted state or provider requests. Cleanup uncertainty,
process failure, malformed output and policy rejection are not retried.
A request-only filter can consume up to nine hello, request and final-shutdown
timeout windows across four processes before dispatch, plus process startup and
cleanup. At `timeout_ms = 30000`, those nine windows alone can take 270 seconds
when no earlier run deadline stops them.

## Version 1 byte exchange

**Startup is required for every configured filter attempt.** Ego starts one
process and first sends a newline-terminated JSON hello without request body,
credentials or URL:

```json
{"version":1,"direction":"hello","generation_role":"main","wire_format":"AnthropicMessages","provider":"example","model":"model","session_id":"uuid","generation_id":"uuid","attempt_id":"uuid","turn_index":0}
```

Only hello carries `generation_role` and `wire_format`. The role is the
resolved generation role: `main`, `judge`, `goal_check`, `compaction`, `subagent`
or `hook`. The wire format names the actual encoder/decoder type, not the
provider ID; for example, `AnthropicMessages` or `OpenAiCompletions`. A provider
alias can therefore share the same wire format. Request and response frames
retain their existing fields. Version remains 1 by explicit owner direction;
strict executables must accept these two hello fields before use.

The executable must flush one JSON response line:

```json
{"version":1,"request":true,"response":false}
```

The two required booleans select interest independently for this attempt.
A missing interest field, absent or malformed response, extra field, or
unsupported version fails closed before provider dispatch;
silence never means pass-through. The declaration cannot change after the
hello. If both directions are declined, Ego closes stdin and requires clean
process exit before dispatch. This still starts the executable to learn
its decision. The previous one-shot v1 request exchange is no longer valid;
update the selected executable before using this contract.

When request interest is true, Ego sends a newline-terminated UTF-8 JSON
request frame on the same process. Its shape is:

```json
{"version":1,"direction":"request","provider":"example","model":"model","session_id":"uuid","generation_id":"uuid","attempt_id":"uuid","turn_index":0,"body_base64":"e30=","state":{"revision":0,"values":{}}}
```

`body_base64` carries the exact normally encoded HTTP body bytes. `model` is
the provider-local model name. `session_id` is the actual session UUID supplied by the session journal owner. Generation and attempt UUIDs come
from the admitted attempt. `turn_index` is zero-based among distinct turns
with a committed user prompt in this session. It is stable for additional
prompts and retries in the same turn, advances for a later prompt turn even if
the prior turn failed, and survives resume. A fork or compaction starts a new
session index; inherited prompts do not count. Automatic checkpoint continuation
occupies index zero; subsequent distinct user turns start at one. Without an
automatic continuation, the first user turn starts at zero. The private request that creates
an ordinary compaction checkpoint uses the index after the source's committed
user turns. It does not add a new user prompt, and the source filter variables
remain available for that request. A source with neither a committed user turn
nor a durable compaction checkpoint cannot run a filtered compaction. In the
new compacted session, automatic continuation from its durable checkpoint uses
index 0 and starts with empty filter variables under the new session UUID. It
does not create a human prompt: the first later committed user turn has index 1.
Input delivered during that automatic turn keeps index 0 and counts only once
when a later turn computes its index. A checkpoint marker without a durable seed is not enough,
and an inherited fork checkpoint does not authorize local continuation.
The response frames repeat the accepted request index.
Ego does **not** supply an original first user message, a prior successful
response ID, URL, request headers, or credentials.
Do not infer missing state from injected system text or assume that a resume
provides it. An addon that needs unavailable facts must reject the request.
The `state` snapshot contains this filter's write-once session variables and revision.

The child must flush one JSON response line:

```json
{"version":1,"body_base64":"e30=","identity_headers":[{"name":"x-request-identity","value":"fixture"}],"expected_revision":0,"state_writes":{}}
```

The returned body is sent as bytes, without parsing or reserialization by Ego.
Transport framing and body-dependent request/cache digests use these final
bytes. Output names are case-insensitive; up to 16 headers from the selected
executable replace existing headers by name, including a credential header.
A replacement value is treated as a public header by Ego; do not assume it
receives the original credential's redaction treatment. Duplicate
names, invalid values, unknown or duplicate JSON fields, missing fields,
unsupported versions, invalid base64, or extra output cause refusal. Header
values must be ASCII HTTP field values of at most 4096 bytes; names are at
most 128 bytes. The executable can emit a beta or routing header for this
attempt; it cannot change the endpoint Ego selected before running the filter.

Both input and output body limits are 16 MiB. Each JSON envelope is limited to
22,435,160 bytes. Child stdout is bounded and stderr is discarded. The command
timeout is also capped by the run deadline. A failed or cancelled filter, a
timeout, or uncertain process cleanup fails closed **before provider dispatch**
for startup and request phases. Ego does not send an unfiltered request as a
fallback after a declared request exchange fails.

Provider filter failures identify the phase (`launch`, `hello`, `request`, or
`response`) and give a category and a next action. For example, `request: early
EOF` means stdout closed before the complete result line; `request: output
limit` means the result exceeded a size limit. `framing` means the child did not
send one valid JSON line; `invalid result` means the line did not satisfy the V1
result contract, including an empty filtered request body. `host` means a local
failure of the session journal or notice sink; the filter is not at fault. The same safe diagnostic is saved in the session error event.
The diagnostic does not include child stderr, bodies, state, or headers. A
response-phase failure does not mean the provider request was unsent.

The filter starts for each actual generation attempt, including an explicitly
retried attempt. A pre-commit command timeout can restart it up to three times
within that same attempt. It receives request bytes only when it declares request interest. Pure
preflight, login, token refresh, model discovery, MCP resource/prompt reads,
and unrelated providers do not invoke the provider exchange. HTTP MCP tool
calls and HTTP / `mcp_tool` hooks use the request-only exchange below. Version 1 refuses WebSocket body rewriting.
CLI and ACP generation use the same model-source binding; a separate ACP host
test is still necessary before claiming host-specific compatibility.

## Request-only external tool and hook checks

When a user filter is enabled, Ego checks HTTP MCP `tools/call` payloads and
HTTP / `mcp_tool` hook payloads by default. Native tools, stdio MCP tool calls,
command hooks, and MCP resource/prompt reads do not receive this exchange.
Prompt hooks still use the normal filtered provider-generation path. Results
are not filtered here: data sent to the next provider generation is already
subject to its provider request filter.

Each dispatch starts a bounded child with one JSON line, then closes stdin
and requires clean process completion. There is no `hello`, response frame,
new generation attempt, or durable state write. The V1 input contains:

- `version: 1`, `direction: "mcp_call"` or `"hook_payload"`;
- `provider`, `model`, `session_id`, `turn_index` from the current session;
- `destination`: MCP server name, HTTP hook URL, or `server/tool` for an MCP hook;
- `body_base64`: UTF-8 JSON payload bytes;
- `state`: a read-only snapshot in the same provider/executable namespace used
  by provider filtering, including previously captured DLP values.

For MCP the JSON is the `tools/call` parameters, including its name, arguments
and metadata. For hooks it is the existing hook event envelope; `mcp_tool`
hooks also include `input`. No transport credentials or HTTP headers are sent.
Only one filter is selected, using the main model's provider override, then
the global fallback; a disabled provider entry opts out. The existing command
timeout, cancellation, run deadline, body/envelope limits and bounded local
notices apply. A session-start hook can receive an empty state before any
provider request has captured values.

An accepted reply has the normal V1 `version`, `body_base64`, `identity_headers`,
`expected_revision`, `state_writes`, and optional `notices` fields. Headers and
writes must be empty and the revision must match the snapshot. The decoded
body must be JSON. A filter can instead return the existing explicit rejection
`{"version":1,"reject":true,"notices":["reason"]}`. Invalid output, timeout,
rejection or uncertain cleanup prevents delivery; there is no unfiltered retry.
A command timeout gets up to three retries with fresh filter processes before
delivery, under the same cancellation and run deadline. The admitted payload
is unchanged; Ego never retries the external tool or hook effect here.

**Egress rewrites fail closed.** The request-only exchange accepts or rejects;
it never rewrites. An HTTP MCP tool call already has an exact-argument permit
and broker binding, and a hook payload is bound to its journaled event. The
filter may accept semantically unchanged JSON or reject it, but cannot change
the admitted call or hook payload. A changed payload is refused with a local
notice, not silently reauthorized. Ego dispatches the original request after
an accept.

Existing custom filters must implement the new directions before they can
accept these external effects. An unsupported direction fails closed. The
shipped DLP example rejects recognized secrets and values from its restored
mapping at these dispatch points; it does not replace remote tool arguments
with placeholders that could change the requested action.

## Local notices and explicit policy rejection

An accepted hello, request or response output can add an optional `notices`
array of strings. For example:

```json
{"version":1,"request":true,"response":false,"notices":["Outbound masking is active"]}
```

Each output allows at most four notices, each at most 1024 UTF-8 input bytes.
At most four nonempty notices are delivered per attempt across all phases;
later notices are suppressed. Notices from a timed-out process that will be
retried are discarded. Before dispatch, Ego buffers notices until it knows that
the process will not be retried, so failed processes cannot consume the accepted
process's notice budget or duplicate its diagnostics.
Ego removes control characters, line separators and common Unicode format
controls, then truncates each notice to 256 UTF-8 bytes. Empty notices are
not shown. The CLI delivers notices through its renderer-owned local diagnostic surface
with the prefix `εgo filter:`. Headless text and JSON-report runs use stderr;
JSONL exposes a separate transient `filter_notice` public event. It is not assistant text, model instructions, a conversation
item or a journal variable. Structured stdout remains protocol-clean. Standard ACP projections omit
filter notices rather than invent assistant text; ACP UI delivery is not
implemented or claimed. Notices are executable-supplied local text,
not a guarantee that the text contains no private data.

Instead of normal output, hello or request can reject with:

```json
{"version":1,"reject":true,"notices":["Request refused by local policy"]}
```

A response rejection must also echo its current sequence:

```json
{"version":1,"reject":true,"sequence":0,"notices":["Response refused by local policy"]}
```

`reject` must be true. Rejection outputs cannot contain a transformed body,
headers, state writes or interest booleans. Unknown or duplicate fields are
refused. Omitting `notices` is permitted. Before HTTP dispatch, rejection
prevents delivery. After dispatch, it stops further response content and
reports delivery as possibly sent; previously published content cannot be
withdrawn. Rejection is a nonretryable local `policy_rejected` failure, which also
prevents advancement to a configured fallback model. Ego never retries
it automatically and never bypasses the selected executable or falls back to
unfiltered bytes. Use a notice for a local explanation; raw child stderr is
still discarded.

## Capture a session variable once

Version 1 includes durable session variables and requires the session journal
owner. It refuses execution without that authority. Its input includes a state snapshot:

```json
{"version":1,"direction":"request","provider":"example","model":"model","session_id":"uuid","generation_id":"uuid","attempt_id":"uuid","turn_index":0,"body_base64":"e30=","state":{"revision":0,"values":{}}}
```

Return the observed revision and proposed string variables with the bytes:

```json
{"version":1,"body_base64":"e30=","identity_headers":[],"expected_revision":0,"state_writes":{"first_value":"captured-once"}}
```

On the next invocation, `state.values.first_value` is `"captured-once"`.
Use the saved value instead of calculating it again. Each value is write-once:
repeating the same write is accepted without another journal record; a different
value is refused. Return an empty `state_writes` object when there is nothing to
add. Keys cannot be deleted or replaced in this contract.

Ego validates the complete body, headers and proposal, then compares the revision
and journals new values **before** HTTP dispatch. A stale revision, failed
journal append or conflicting write prevents delivery; a later fresh attempt
must read the current snapshot. Variables survive normal `resume` and continuation
of the same session. A new session, fork, provider or configured executable path
has a separate namespace. Replacing the executable at the same path retains
that namespace. Disabling the filter retains its saved values; re-enabling the
same path in the same session restores them.

State accepts at most 64 keys per namespace. Names contain ASCII letters,
digits, `_`, `-` or `.`, are nonempty and at most 128 bytes. String values are
at most 4096 UTF-8 bytes each; the complete encoded values object is at most
64 KiB. The existing envelope and execution limits still apply, including
the added state. State lives privately in the session journal; exports omit
names and values. Values are stored in the private local journal, without encryption added by
this feature. A DLP mapping can contain the original sensitive content; it must
be treated with the same care as the local conversation journal. No original-first-prompt or prior
provider-response identity is invented by this API.

The essential Python pattern inside a version-1 executable is:

```python
hello = json.loads(sys.stdin.readline())
assert hello["version"] == 1 and hello["direction"] == "hello"
print(json.dumps({"version": 1, "request": True, "response": False}), flush=True)
request = json.loads(sys.stdin.readline())
saved = request["state"]["values"]
value = saved.get("first_value")
writes = {}
if value is None:
    value = capture_from_this_first_interaction(request)
    writes["first_value"] = value
print(json.dumps({
    "version": 1,
    "body_base64": request["body_base64"],
    "identity_headers": [],
    "expected_revision": request["state"]["revision"],
    "state_writes": writes,
}), flush=True)
assert sys.stdin.read(1) == ""  # Ego closes stdin before dispatch.
```

`capture_from_this_first_interaction` is your addon's capture logic. This
example declares request-only interest; a reversible policy must declare both
and keep reading response frames. The process
does not need globals, turn files or its own persistence directory.

## Response stream exchange

When response interest is true, the same process stays alive after the hello
and optional request exchange. It reads response JSON lines and keeps stream
context until EOF. When response interest is false, Ego closes and checks the
process before HTTP dispatch and sends response bytes directly to the provider
decoder. Each filtered response input repeats the accepted, read-only state
snapshot and attempt context:

```json
{"version":1,"direction":"response","provider":"example","model":"model","session_id":"uuid","generation_id":"uuid","attempt_id":"uuid","turn_index":0,"body_base64":"ZGF0YToge31cblxu","state":{"revision":1,"values":{"captured":"value"}},"sequence":0,"eof":false,"status":200}
```

Return exactly one JSON line for each input, and flush stdout:

```json
{"version":1,"sequence":0,"body_base64":"ZGF0YToge31cblxu"}
```

Sequences start at zero and increase by one. `body_base64` contains raw incoming
transport bytes, not decoded assistant text. Empty output is allowed so the
filter can retain incomplete UTF-8, SSE or placeholder fragments. HTTP chunks
are not message boundaries. The executable owns provider protocol parsing and
must apply any restoration inside both assistant text and tool-argument strings.
Ego has no DLP policy or built-in placeholder replacement.

A final frame has `eof:true` and empty input bytes. Flush retained data in its
response, then wait for stdin to close and exit successfully. Ego validates exit
and process-tree cleanup before publishing the final output. Extra stdout,
unknown fields, state writes, headers, incorrect sequences or invalid envelopes
fail closed. Failures after dispatch remain possibly sent and are local failures;
Ego never falls back to unfiltered response bytes. Previously emitted streamed
content cannot be retracted.

The existing size limits apply to each response frame. The command timeout
bounds each exchange and final shutdown; it does not count time awaiting the
provider's next chunk. Run cancellation/deadline and process ownership remain
active during that idle interval. Dropping the response stream cancels its child.
The selected executable must support startup and each direction it declares.
A DLP executable that restores placeholders must declare response interest;
otherwise returned placeholders will not be restored.

## Reversible DLP example

[`tools/protocol-filter/dlp.py`](../../tools/protocol-filter/dlp.py) is an
executable example for **Anthropic Messages**. Its request rules replace
recognized secrets in JSON string values with random placeholders:

- PEM private keys, `sk-` tokens and email addresses.
- GitHub classic/fine-grained PATs and OAuth/App tokens, including dotted
  installation tokens; common GitLab PAT, deploy, runner and CI token prefixes.
- npm/PyPI tokens, Slack tokens, Stripe secret/restricted keys, Google API keys,
  AWS `AKIA`/`ASIA` access-key IDs and JWT-shaped strings.
- Labeled password, token, API-key and secret values in JSON and pasted
  `.env`, `.npmrc`, AWS/configuration and log content; Basic/Bearer authorization
  values, URL user/password credentials, and selected signed URL parameters.

The executable examines the request data; it does not open credential files.
Context rules preserve assignment names, URL hosts and unrelated query parameters.
Overlapping captures store complete originals rather than nested placeholders.
Known values are masked throughout the request, including occurrences preceding
their labeled capture. If masking would alter provider controls, message roles,
content-block tags or tool declarations/schemas, the example refuses the request
instead of silently changing the protocol. This can happen when a short or common
captured value also occurs in those structural fields.
The rules are conservative heuristics, not credential validation: custom token
prefixes, encoded/obfuscated content and unrecognized formats can escape detection;
example values with matching shapes may also be masked. Arbitrary hashes, UUIDs,
public URLs and file paths alone are not treated as secrets.

It saves the original values through `state_writes` and reuses the same
placeholders on later interactions and resume. Previously captured originals are
also masked when returned in later history without their original label.
Add and review policy rules for your own data before use.

Token-prefix references: [GitHub](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/about-authentication-to-github),
[GitLab](https://docs.gitlab.com/security/tokens/) and
[AWS identifiers](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_identifiers.html).

### Provider cache and accounting

For the same request and persisted mapping, the example produces identical
outbound bytes after resume. New placeholders are created only for newly
captured values. The response transformation preserves provider `usage` events,
including cache creation/read counters; it does not recalculate billed tokens
from restored local text. A restored secret can have a different token length
from the placeholder the provider actually processed. Local pre-dispatch token
estimates need not match that transformed request.

Cache hits are not guaranteed. Enabling the filter on existing history, changing
its rules, or starting an isolated session with different placeholders can change
the cached prefix. Anthropic requires an exact matching cached prefix, subject
to its normal size, TTL and other rules. See
[Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching).
Offline stability and counter-preservation tests do not establish a live cache hit.

On incoming SSE it restores known placeholders in text and tool-argument
deltas, including placeholders split across transport chunks and content
deltas. The mapping remains local; the provider sees only placeholders for
values matched by the example's rules. Restored values can reach local tools,
output and the normal local journal. The request-only external check rejects
known restored values before HTTP MCP and HTTP / `mcp_tool` hook delivery.
This example does not create an encrypted
secret vault. Oversized secrets or mappings fail before dispatch.

Example user configuration (replace the absolute path):

```toml
[auth.anthropic.protocol_filter]
command = "/absolute/path/to/ego/tools/protocol-filter/dlp.py"
timeout_ms = 10000
```

The Python executable needs no third-party dependencies. Its interpreter must
be available at the shebang path. The filter is not enabled by default. Validate
it with synthetic values and a controlled loopback provider first.

Test with a controlled loopback provider before using a real provider. Compare
the provider's received body bytes to the addon's returned decoded bytes; check
which headers changed and whether the filter replaced Ego's original
authentication header. Then try a disabled filter, a second provider, invalid output,
a nonzero exit, a timeout, cancellation, and a retry. A successful HTTP status
alone does not prove a complete useful response, subscription entitlement, or
native-client protocol parity. Do not install historical capture/verifier
scripts as runtime dependencies or enable a vendor-specific identity addon
without separate authorization and current-version evidence.

See the [configuration reference](../reference/configuration.md#provider-protocol-filter)
for the exact configuration.
