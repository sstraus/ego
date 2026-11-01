# Provider protocol filter

A `protocol_filter` is an optional, user-owned executable that changes the outgoing HTTP
body and incoming streamed bytes of one provider generation attempt. It can also return declared, non-secret
identity headers. Ego runs its request exchange after normal request encoding and before HTTP
dispatch. A persistent response process transforms incoming bytes before provider decoding. It does not act as an HTTPS proxy, credential source, general
plugin system, or provider compatibility guarantee.

This is a **provider transport hook** with outgoing-request and streamed-response
phases. Content filtering and reversible DLP are policies implemented by the
selected executable; its configuration key remains `protocol_filter`.
The supported handler is a local CLI executable. This filter remains separate
from lifecycle hooks and does not support their HTTP, MCP or prompt handlers.

Use this feature only with an executable that you have reviewed. The executable
runs with your OS account. Ego clears its inherited environment, but that does
not prevent the executable from reading your files or opening its own sockets.
The request body can contain private conversation content.

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
   [auth.example.protocol_filter]
   command = "/absolute/path/to/my-filter"
   identity_headers = ["x-request-identity"]
   timeout_ms = 1000
   enabled = true
   ```

3. Start a new session to load the saved configuration. To turn the filter off,
   remove the table or set `enabled = false`, then start a new session. No
   shipped provider default or model selection changes when you configure it.
   The managed `ego config set` key list does not include this table; use
   `ego config edit` or edit the user file and validate the complete TOML.

`command` is one absolute executable path, not a shell command. Ego supplies no
arguments or working-directory override. `identity_headers` is an allowlist,
not a request to emit each named header. It can contain at most 16 distinct
names. `timeout_ms` is 1 through 1000, with a default of 1000. `enabled`
defaults to true. A complete provider filter entry is selected from the user
layer; executable fields are not merged with other layers.

## Version 1 byte exchange

Ego sends one UTF-8 JSON object to child stdin and closes stdin. The shape is:

```json
{"version":1,"direction":"request","provider":"example","model":"model","session_id":"uuid","generation_id":"uuid","attempt_id":"uuid","body_base64":"e30=","state":{"revision":0,"values":{}}}
```

`body_base64` carries the exact normally encoded HTTP body bytes. `model` is
the provider-local model name. `session_id` is the actual session UUID supplied by the session journal owner. Generation and attempt UUIDs come
from the admitted attempt. Ego does **not** supply an original first user
message, a prior successful response ID, URL, request headers, or credentials.
Do not infer missing state from injected system text or assume that a resume
provides it. An addon that needs unavailable facts must reject the request.
The `state` snapshot contains this filter's write-once session variables and revision.

The child must write one JSON object to stdout:

```json
{"version":1,"body_base64":"e30=","identity_headers":[{"name":"x-request-identity","value":"fixture"}],"expected_revision":0,"state_writes":{}}
```

The returned body is sent as bytes, without parsing or reserialization by Ego.
Transport framing and body-dependent request/cache digests use these final
bytes. Ego keeps its own credential headers. Output names are case-insensitive;
only allowlisted non-secret identity headers can replace an existing public
header. Duplicate names, forbidden names, invalid values, unknown or duplicate
JSON fields, missing fields, unsupported versions, invalid base64, or extra
output cause refusal. Header values must be ASCII HTTP field values of at most
4096 bytes; names are at most 128 bytes. Authentication, cookies, routing,
framing, content negotiation, version, and beta headers cannot be added even if
listed in the configuration.

Both input and output body limits are 16 MiB. Each JSON envelope is limited to
22,435,160 bytes. Child stdout is bounded and stderr is discarded. The command
timeout is also capped by the run deadline. A failed or cancelled filter, a
timeout, or uncertain process cleanup fails closed **before provider dispatch**;
Ego does not send the unfiltered request as a fallback.

The filter runs once per actual generation attempt, including an explicitly
retried attempt. Pure preflight, login, token refresh, model discovery, MCP, and
unrelated providers do not invoke it. Version 1 refuses WebSocket body rewriting.
CLI and ACP generation use the same model-source binding; a separate ACP host
test is still necessary before claiming host-specific compatibility.

## Capture a session variable once

Version 1 includes durable session variables and requires the session journal
owner. It refuses execution without that authority. Its input includes a state snapshot:

```json
{"version":1,"direction":"request","provider":"example","model":"model","session_id":"uuid","generation_id":"uuid","attempt_id":"uuid","body_base64":"e30=","state":{"revision":0,"values":{}}}
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
request = json.load(sys.stdin)
saved = request["state"]["values"]
value = saved.get("first_value")
writes = {}
if value is None:
    value = capture_from_this_first_interaction(request)
    writes["first_value"] = value
json.dump({
    "version": 1,
    "body_base64": request["body_base64"],
    "identity_headers": [],
    "expected_revision": request["state"]["revision"],
    "state_writes": writes,
}, sys.stdout)
```

`capture_from_this_first_interaction` is your addon's capture logic. The process
does not need globals, turn files or its own persistence directory.

## Response stream exchange

The same executable starts a second process per attempt. It reads JSON lines
and keeps stream context until EOF. Each input repeats the accepted, read-only
state snapshot and attempt context:

```json
{"version":1,"direction":"response","provider":"example","model":"model","session_id":"uuid","generation_id":"uuid","attempt_id":"uuid","body_base64":"ZGF0YToge31cblxu","state":{"revision":1,"values":{"captured":"value"}},"sequence":0,"eof":false,"status":200}
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
The selected executable must support both directions of the single v1 contract.

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
output and the normal local journal. This example does not create an encrypted
secret vault. Oversized secrets or mappings fail before dispatch.

Example user configuration (replace the absolute path):

```toml
[auth.anthropic.protocol_filter]
command = "/absolute/path/to/ego/tools/protocol-filter/dlp.py"
timeout_ms = 1000
```

The Python executable needs no third-party dependencies. Its interpreter must
be available at the shebang path. The filter is not enabled by default. Validate
it with synthetic values and a controlled loopback provider first.

Test with a controlled loopback provider before using a real provider. Compare
the provider's received body bytes to the addon's returned decoded bytes; check
that only declared identity headers changed and Ego still supplies its own
authentication. Then try a disabled filter, a second provider, invalid output,
a nonzero exit, a timeout, cancellation, and a retry. A successful HTTP status
alone does not prove a complete useful response, subscription entitlement, or
native-client protocol parity. Do not install historical capture/verifier
scripts as runtime dependencies or enable a vendor-specific identity addon
without separate authorization and current-version evidence.

See the [configuration reference](../reference/configuration.md#provider-protocol-filter)
for the exact configuration.
