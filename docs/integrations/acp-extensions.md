# ego extensions to ACP

This is the client integration contract audited on 2026-10-10 and reconciled
against the source, most recently on 2026-10-31. It describes ego's implemented
contract, not a promise that every ACP client implements it.
`C → E` means client to ego. `E → C` means ego to client.

## Comparison baseline

The comparison uses the official [ACP v1 specification](https://agentclientprotocol.com/protocol/v1/overview),
[extensibility rules](https://agentclientprotocol.com/protocol/v1/extensibility),
and [transport rules](https://agentclientprotocol.com/protocol/v1/transports).
The official repository was read with `gh api`, at commit
`f4b7c8d261d71c8bd8b3ea1918dbc0f2cef01d7f`:

- [Stable v1 JSON schema](https://github.com/agentclientprotocol/agent-client-protocol/blob/f4b7c8d261d71c8bd8b3ea1918dbc0f2cef01d7f/schema/v1/schema.json).
- [Schema release marker](https://github.com/agentclientprotocol/agent-client-protocol/blob/f4b7c8d261d71c8bd8b3ea1918dbc0f2cef01d7f/schema/v1/Cargo.toml): **1.25.0**.
- [Method map](https://github.com/agentclientprotocol/agent-client-protocol/blob/f4b7c8d261d71c8bd8b3ea1918dbc0f2cef01d7f/schema/v1/meta.json): protocol **1**.

Protocol version and schema package version are different numbers. ego serves
protocol **1**. It depends on Rust SDK 2.0.0, schema crate 1.5.0, and HTTP adapter
2.0.0. See `crates/ego-acp/Cargo.toml:10` and
`crates/ego-acp/src/negotiate.rs:49`. The current upstream site also has v2
pages; their presence does not change the version ego serves. The code's older
comments about upstream draft status are not a fresh upstream status check.

ACP permits custom data in `_meta` and custom method names that start with `_`.
ego puts its data inside `_meta.ego`; it does not add ego fields to the root of
standard messages. Unrecognized custom request names receive method-not-found. Read the
advertisement before calling a custom method.

### Inventory and counting rule

The inventory below groups capability keys, session input/output metadata,
custom requests, and custom errors. Nested members and repeated uses of one key
are not separate operations. The implemented custom requests are `_ego/steer`,
`_ego/compact`, `_ego/pause`, `_ego/resume`, and `_ego/retry`. Session inputs also
include `systemPrompt`, `bare`, and deletion `force`; these are not independent
capability advertisements. Read each section's negotiation and fallback rules
instead of using a historical entry count as a feature test.

Transport policy, permission choices, and standard-field interpretations are
listed separately below. There are no ego-named notification methods in the
router: extensions are carried by standard `session/update` notifications.

JSON examples with `S`, `R`,
`T`, or `M` stand for valid non-nil session, request, target, or message UUIDs;
replace these labels before sending a request. Examples of metadata show the
containing standard object fragment, not a complete JSON-RPC envelope.

## Advertised capabilities

The production durable composition advertises:

```json
{
  "loadSession": true,
  "promptCapabilities": {"image": true, "audio": true, "embeddedContext": true},
  "mcpCapabilities": {"http": true, "sse": true, "acp": true},
  "sessionCapabilities": {
    "list": {}, "delete": {"_meta": {"ego": {"force": true}}}, "resume": {}, "close": {},
    "fork": {"_meta": {"ego": {"atMessage": true}}}
  },
  "_meta": {"ego": {
    "steer": {"version": 1, "method": "_ego/steer", "contentTypes": ["text"]},
    "retry": {"version": 1, "method": "_ego/retry"},
    "childActivity": {"version": 1, "update": "session_info_update", "field": "_meta.ego.childActivity"},
    "compact": {"version": 1, "method": "_ego/compact"},
    "hold": {"version": 1, "pause": "_ego/pause", "resume": "_ego/resume"}
  }}
}
```

`_meta.ego.steer`, `.retry`, `.compact`, `.hold`, and the fork capability's
`_meta.ego.atMessage` are E → C capability extensions. `steer` is always
advertised by this router. `compact` requires the compaction port. `hold`,
`loadSession`, and the session lifecycle capabilities require the lifecycle
port. `atMessage` is a boolean, not a method or a version number. There is no
separate `pause` capability key. An absent port must not become an empty but
functional control. A plain client ignores these metadata keys and uses only
advertised standard methods. An extension-aware client hides unavailable
controls; it must not infer support from the ego product version.

Errors and tests for these capability entries are the corresponding method
errors and tests below. Absence tests:
`host_hold.rs::an_agent_without_a_lifecycle_neither_advertises_nor_serves_hold`
and `compaction_extension.rs::an_agent_without_the_compaction_port_neither_advertises_nor_serves_compact`.
`session/fork` and MCP-over-ACP are supplied through unstable features of ego's
pinned SDK. In the compared stable upstream schema, `session/fork` and
`mcpCapabilities.acp` are not standard v1 entries. They are upstream draft
surfaces, not additional ego-named methods.

## Custom requests and errors

The custom requests are C → E, with an E → C JSON-RPC result. They require
`v: 1`. Their request structs reject unknown fields. A response `v` is the
extension version, not the negotiated ACP version.

### `_ego/steer`

```json
{"jsonrpc":"2.0","id":42,"method":"_ego/steer","params":{"v":1,"sessionId":"S","prompt":[{"type":"text","text":"Prioritize the new constraint"}]}}
```

Result examples are `{"v":1,"state":"accepted"}`,
`{"v":1,"state":"not-busy"}`, or
`{"v":1,"state":"rejected","reason":"steering text is empty"}`.
Text blocks are joined with newlines. Non-text blocks and empty text are
rejected. `accepted` confirms durable FIFO admission into the current turn;
consumption occurs at a safe generation boundary after the tool batch settles.
The consumed input is a standard `user_message_chunk` and durable history.
It does not start a second turn. Cancellation does not erase accepted input.
There is no request-id deduplication for this method.

Errors: invalid version or malformed session ID is invalid-params; an unopened
session is refused. Unsupported content is a `rejected` result, not an error.
Fallback: when the capability is absent, wait for the turn and use
`session/prompt`. On `not-busy`, send the original content once with
`session/prompt`. Do not automatically resend `accepted` text after cancellation
or an uncertain transport result.

### `_ego/compact`

```json
{"jsonrpc":"2.0","id":43,"method":"_ego/compact","params":{"v":1,"sessionId":"S","requestId":"R"}}
```

```json
{"v":1,"sourceSessionId":"S","sourceSeq":42,"targetSessionId":"T","publication":{"kind":"published_durably","diagnostic":null},"successorStartRequestId":"R"}
```

The shared compaction service creates a successor with a durable checkpoint.
The receipt names the source boundary and successor. `publication` is the
core publication enum: `not_published`, `published_durably`, or
`published_durability_uncertain`, with its diagnostic. Do not assume that an
uncertain publication is a durably completed transition. The successor can
accept the next standard prompt. Profile authority is restored for the source
and carried to the successor.

Errors: absent port is method-not-found; bad version, nil or malformed UUIDs
are invalid-params; service failure is internal-error. Profile restoration can
also refuse the request. Fallback: no standard compaction request is served by
this build. Hide the control or start a separate chat; do not treat cancellation
as compaction.

### `_ego/pause` and `_ego/resume`

```json
{"jsonrpc":"2.0","id":44,"method":"_ego/pause","params":{"v":1,"sessionId":"S","requestId":"R"}}
```

```json
{"v":1,"sessionId":"S","requestId":"R","state":"paused"}
```

For release, use the same shape with method `_ego/resume` and a new request ID;
a successful release reports `state: "running"`. Pause reports `paused` only
at a durable boundary. It reports `pending` while an in-flight effect must
settle. Pending does not promise a crash-persistent hold. Durable holds survive
connection loss. Release of a pending active turn is refused; release of a
session with no hold is invalid-params. Both methods replay their recorded
answers for the same session, method, and request ID, even after state changes.

Errors: absent lifecycle is method-not-found; bad version, unknown fields, nil
or malformed UUIDs are invalid-params. Missing/deleted sessions use standard
resource-not-found (`-32002`). Active, leased, or conflicting lifecycle state
uses `-32010`. Internal failures use internal-error. Fallback: hide hold
controls when the advertisement is absent. Standard `session/resume` is a
history-free attachment; it is **not** the release operation. Standard cancel
is not pause.

### Error codes `-32010` and `-32011`

These are E → C ego server errors, not standard ACP error assignments.

```json
{"jsonrpc":"2.0","id":45,"error":{"code":-32011,"message":"the session is held; continue it with `_ego/resume`"}}
```

`-32010` means a transient lifecycle busy/lease/conflict refusal. Retry safety
still depends on the operation: hold requests have durable request IDs;
steering does not. `-32011` means a held prompt or a prompt that reached a pause
boundary. Offer `_ego/resume`, not an automatic prompt retry. Do not parse
error text as the discriminator. A plain client can show the error but cannot
release a hold without the extension. When these codes are absent, follow the
standard error contract; do not infer an ego hold from generic cancellation.

## Session creation metadata

### `_meta.ego.trust`

C → E on `session/new`:

```json
{"cwd":"/workspace/repo","mcpServers":[],"_meta":{"ego":{"trust":"trusted"}}}
```

The exact string `trusted` attests trust for the canonical workspace. Production
honors it only with a launch profile explicitly selected through flags. A
per-session `profile` alone does not enable host attestation. The shared trust
ledger records the current executable fingerprint; changed executables can
invalidate the answer.

Errors: other values are ignored by this reader, not interpreted as consent.
Failure to record an attestation logs a warning; it does not grant remembered
trust. A non-object `_meta.ego` is refused by the session-profile parser.
Fallback: omit the key and use normal form trust elicitation. Without form
support the session stays untrusted; there is no terminal-input fallback.

### `_meta.ego.profile`, `.ceilingProfile`, `.effective`, `.warnings`

`profile` and `ceilingProfile` are optional C → E profile-name strings on
`session/new`. All four keys can occur E → C in its result:

```json
{"cwd":"/workspace/repo","mcpServers":[],"_meta":{"ego":{"profile":"repo","ceilingProfile":"machine"}}}
```

```json
{"sessionId":"S","_meta":{"ego":{"profile":"repo","ceilingProfile":"machine","effective":{"mode":"default","sandbox":"workspace","roots":[],"execPaths":[]},"warnings":["mode yolo exceeds profile ceiling default; using default"]}}}
```

The session profile overrides the launch profile. The optional ceiling resolves
from machine configuration layers, not workspace overrides. The clamp keeps
narrower mode and confinement, drops roots outside the ceiling, downgrades
write roots above read-only bounds, and bounds executable roots. The workspace
remains admitted. Administrator ceilings still apply. `effective` reports mode,
sandbox, roots, and executable paths. `warnings` is an ordered string array;
an unchanged request has an empty array. Returned names can be null when no
name was selected for that side of the configuration.

Creation records names before the first prompt. Load/resume and successor
lineage restore that authority. A missing ceiling on reopening is a refusal,
not permission to remove it. A live profile seat does not silently accept a
changed profile policy.

Errors: non-object ego metadata, malformed names, unknown profiles, unsupported
profile admission, or invalid restored policy refuse the operation. A host
clamp itself succeeds with warnings. Fallback: omit both input keys to retain
launch behavior; without `ceilingProfile`, a profile adds no host clamp. If the
result keys are absent, do not assume that a host ceiling was applied. There
is no capability key for profile support in the current advertisement.

## Session and history metadata

All entries in this section travel E → C unless an input is explicitly named.
Output metadata alone does not grant authority. Plain clients can ignore it.

### `_meta.ego.atMessageId` and fork `.atMessage`

C → E in `session/fork`; `.atMessage` advertises this support in the fork
capability, as shown above:

```json
{"sessionId":"S","cwd":"/workspace/repo","mcpServers":[],"_meta":{"ego":{"atMessageId":"M"}}}
```

The message UUID selects the boundary after its successful completed turn.
Omission selects the standard tip fork. Invalid or unavailable cutoffs never
fall back to the tip. Errors are invalid-params for malformed UUIDs or invalid
fork boundaries, with lifecycle errors for missing/busy source sessions.
Fallback: without `.atMessage`, offer only tip forks if `fork` itself exists.
Do not send a message selector to an agent that has not advertised it.

### `_meta.ego.hold.state`, `.summary`, and `.lineage`

These keys occur in `session/list` rows, the `session/load` result, and
`session_info_update`. The capability `.hold` object is a different use of the
same top-level key. Example of an info update:

```json
{"sessionUpdate":"session_info_update","title":"Cache fix","_meta":{"ego":{"hold":{"state":"paused"},"summary":"Fix the cache diff.","lineage":{"kind":"fork","sourceSessionId":"S","rootSessionId":"S","sourceDeleted":false,"atMessageId":"M"}}}}
```

- `hold.state`: `running`, `pending`, or `paused`, from core lifecycle state.
  Do not treat pending as a durable pause.
- `summary`: optional durable synopsis. The normal completed prompt/answer
  synopsis has at most 300 UTF-8 bytes. Compaction uses the checkpoint synopsis.
  It is not a provider retry label or title replacement.
- `lineage`: optional child relationship. `kind` is `fork` or `compaction`;
  `sourceSessionId` and `rootSessionId` are UUIDs; `sourceDeleted` is a boolean.
  Fork lineage can include `atMessageId`, the canonical cutoff marker,
  including for tip forks. Deleting a parent does not delete inherited history.

Errors: reading durable metadata can fail with lifecycle errors; absent summary
or lineage is omitted, not a failed session. An unusable synopsis does not fail
the completed turn. Fallback: use standard title and `updatedAt` for the picker;
show no tree or hold status when the keys are absent. Absence of hold metadata
is not evidence that a held session was released.

### `_meta.ego.inherited`

On replayed message/thought chunks and tool creation/update objects inherited
from an ancestor:

```json
{"sessionUpdate":"user_message_chunk","messageId":"M","content":{"type":"text","text":"Original request"},"_meta":{"ego":{"inherited":true}}}
```

Ancestor history appears before local history, with original message IDs.
Compaction checkpoints stop ancestor transcript traversal. Usage remains local
to the child. Errors are replay/lifecycle errors, not a new metadata error.
Fallback: render the standard messages in arrival order; do not require this
key to display them. Its absence does not prove that a session has no ancestry.

## Live update metadata

### `_meta.ego.providerRetry`

E → C, as a `session_info_update` patch:

```json
{"sessionUpdate":"session_info_update","_meta":{"ego":{"providerRetry":{"state":"waiting","attempt":2,"limit":6,"delayMs":1500,"kind":"transient","cause":"Server overloaded","text":"Server overloaded — connection problem, retrying in 2s (attempt 2/6)"}}}}
```

The status reports the next retry wait. Display text rounds milliseconds up to
seconds. A later patch sets `providerRetry` to **null** to clear the status when
an item starts, changes, completes, or the turn completes. Merge this as a
property patch; do not erase the title, lineage, or other ego metadata. It is
not transcript content. Errors: notification delivery can fail; the sink logs
the first delivery failure and does not manufacture receipt. Fallback: ignore
the key and retain standard session state. Absence of this extension cannot
show whether a provider is waiting.

### Recovery and `_ego/retry`

Interactive and ACP defaults can recover for 24 hours after the first safe
transient failure. `providerRetry` adds `phase` (`recovery` or `finite`),
`recoveryRemainingMs` (null for finite plans), and
`force: {"method":"_ego/retry","version":1}`. In recovery, `attempt` is an
increasing counter, not a value bounded by the legacy finite `limit` field.
Use `phase` and `recoveryRemainingMs` to render recovery rather than an
exhausted finite allowance. `delayMs` is the next actual wait, including jitter.
Local waits grow to 120 seconds; provider waits are not shortened.

The initialize capability `_meta.ego.retry` advertises the version and method.
A host, including TUIC, can send this request while `session/prompt` is pending:

```json
{"jsonrpc":"2.0","id":42,"method":"_ego/retry","params":{"v":1,"sessionId":"SESSION_UUID"}}
```

The result is `{"v":1,"state":"accepted"}` only when a live provider backoff
is released. Idle sessions and active streams return `not-waiting`; a closed
coordinator also returns `not-waiting`. Invalid versions or session IDs are
invalid parameters. The request never cancels an active stream, resets the
horizon, bypasses cancellation/budget checks, or replays an attempt after
output. It is a control request, not a new prompt. Parent-state rendering in
TUIC is outside this protocol implementation.

### `_meta.ego.salience` and `.action`

E → C on standard updates. Tool creation/progress/completion and reasoning use
`salience: "activity"`. Out-of-turn notices use an agent message with
`salience: "card"` and an action:

```json
{"sessionUpdate":"agent_message_chunk","messageId":"card-1","content":{"type":"text","text":"Worker result is ready"},"_meta":{"ego":{"salience":"card","action":{"kind":"open_result","path":"/workspace/result.md"}}}}
```

Action shapes are `{"kind":"open_result","path":"..."}`,
`{"kind":"answer","question_id":"..."}`, and
`{"kind":"approve","request_id":"..."}`. The underscore spelling of action
members is intentional. Activity metadata carries no action. A client can
collapse activity and show cards without starting a model turn. Card IDs support
client deduplication. Do not interpret an `approve` action as permission already
granted or automatically execute a path from an `open_result` action.

Errors: notices require an attached session route and delivery recorder. A
closed route refuses delivery. Journal failure after enqueue reports that
failure; it does not prove the host did not see the card. Fallback: render the
ordinary ACP message/tool/thought update; do not require an ego action handler.
No custom notification name is involved.

### `_meta.ego.goalId` and `.goalState`

E → C on the single plan entry for the current durable goal:

```json
{"sessionUpdate":"plan","entries":[{"content":"Fix the cache","priority":"medium","status":"pending","_meta":{"ego":{"goalId":"M","goalState":"blocked"}}}]}
```

`goalId` identifies the goal. `goalState` is `active`, `achieved`, or `blocked`.
Standard status is respectively `in_progress`, `completed`, or `pending`.
Attachments and live turn boundaries publish the full entry, not invented
substeps. No goal produces no goal plan update. Errors are lifecycle read or
notification delivery errors. Fallback: show the ordinary plan entry and its
standard status; without metadata, `pending` does not distinguish a blocked
ego goal from other pending work.

## Standard vs ego

This table includes standard-field interpretations and deployment policies.
They are not extra `_meta.ego` keys. Fallback means a safe client action, not a
second protocol implementation.

| Surface and direction | Standard baseline | ego shape, semantics, errors, and fallback |
| --- | --- | --- |
| `initialize`, C → E → C | Negotiate a supported protocol version; report capabilities. | `{"protocolVersion":1}` for supported asks at or above 1. Version 0 is refused by the SDK. Do not assume that compiled v2 projections are served. Use the returned version or refuse the connection. |
| `cwd`, C → E on session setup | Absolute path required by the compared schema. | Production also accepts a relative path, for example `{"cwd":"repo","mcpServers":[]}`, relative to the launch base. It canonicalizes and requires an existing directory. Load/resume require the durable session root to match. Bad roots are invalid-params. Portable clients must send an absolute path. This is a permissive difference, not an advertised extension. |
| `additionalDirectories`, C → E | Optional extra roots, subject to capability negotiation. | Any non-empty array, such as `{"additionalDirectories":["/other"]}`, is invalid-params before root resolution or MCP effects, for new/load/resume/fork. Omit it or send `[]`; it is not a way to loosen the profile perimeter. |
| `mcpServers`, C → E | Client supplies MCP server descriptions. HTTP/SSE are capability-gated. | Example stdio entry: `{"name":"tools","command":"/usr/bin/tool-server","args":[],"env":[]}`. ego requires absolute stdio commands, valid case-insensitively unique names/env names, valid HTTP URLs and headers, no duplicate or wire-owned HTTP headers, and non-empty ACP server IDs. Plain HTTP is limited to exact loopback. Explicit SSE selects the legacy profile; unknown transports refuse; the raw-versus-parsed count check prevents silently dropped servers. Invalid selection is refused atomically. Omit unsupported servers rather than assume partial admission. |
| `session/prompt` content, C → E | Text and resource links are baseline; images are advertised. | `[{"type":"resource_link","uri":"file:///workspace/a","name":"a"}]` becomes URI text, not an automatic file read. Text joins with newlines. Embedded text accepts absent MIME or text/plain, text/markdown, text/html, text/csv, application/json; binary resources accept JPEG/PNG/GIF/WebP only. Host bytes are used directly: no file read or URI fetch. Image positions survive durable blob metadata as UTF-8 text offsets and provider reconstruction. Resource provenance is a URI digest; each embedded resource is explicitly labeled host-supplied untrusted data, not instructions. Raw URI, annotations and `_meta` are discarded. See [IDE setup](ide.md) for selection, diagnostic and edit-review snapshots. URI metadata must be nonempty, at most 4096 bytes and free of control characters. Joined text is limited to 1 MiB; images to 100 and 64 MiB total decoded bytes. JPEG/PNG/GIF/WebP require valid base64 and matching decoded media. Audio accepts explicit `audio/wav` blocks only: at most four clips and 10 MiB total, with valid PCM16 WAV bytes (mono/stereo, at most 48 kHz and 60 seconds per clip). Mixed audio/image prompts and unsupported resource media refuse before a turn. The selected provider/model must support the admitted audio contract. Model limits still apply. Send text-only if image support is absent. |
| Prompt admission, C → E | One prompt request settles its turn. | A second simultaneous prompt is invalid-request, not a queue. Unopened sessions are invalid-params. Correlation uses request content digest as well as identity; changed content is new work. Use advertised steering for busy input, otherwise wait. |
| Load/resume, C → E → C | Load replays history; resume does not. | `{"sessionId":"S","cwd":"/workspace/repo","mcpServers":[]}` reattaches durable execution and restores creation profile authority. Load replays ordered projectable history and at most one final reduced usage update; resume emits current info but no history/usage replay. Same-connection active replacement refuses. Uncertain effects require close/new, not replay. With no resume capability, use advertised load; never use standard resume to release a hold. |
| Permission options, E → C request, C → E answer | Option IDs are opaque; option kind indicates allow/reject once/always. | `{"outcome":{"outcome":"selected","optionId":"allow-session"}}` grants transient session approval, despite its standard kind `allow_always`. IDs `allow-once`, `reject-once`, `allow-session`, and when rememberable `allow-always`/`reject-always` are distinct. Remembered answers write displayed user rules in the active profile. Cancellation, unknown IDs, no seat, transport failure, or persistence failure deny. No timeout is imposed on a legitimate open permission question. Render offered labels and return the exact ID; never infer persistence from kind alone. Fallback is once-only choices when that is all the agent offers, not terminal input. |
| Sandbox path permissions, E → C request, C → E answer | Standard permission response envelope. | `{"outcome":{"outcome":"selected","optionId":"sandbox-session"}}` permits the shown path for this attachment. `sandbox-always` persists the workspace grant; `sandbox-deny` refuses. The question shows the command that will run again. Transport/unknown answer/persistence failures deny. Ordinary tool approval does not imply a sandbox path grant. Render the offered choices; do not synthesize an unoffered grant. |
| Config options, E → C offer, C → E selection | Standard select controls and optional categories. | `{"sessionId":"S","configId":"sandbox","value":"workspace"}` selects only a currently offered value. IDs are `model`, `effort`, `mode`, `sandbox`; sandbox uses category `sandbox`, not mode. `__ego_unselected__` is an offered no-op only while model/effort is unset. Boolean, unoffered values, and unknown IDs refuse. Absent seat is method-not-found. Full replacement options return and publish after acceptance; mode also publishes current-mode update. Sandbox can narrow but not exceed the ceiling and cannot change after stdio MCP pins it. Read the offer; do not hard-code choices or send `session/set_mode`. |
| Durable model selection, C → E → C | Config selection changes the session model. | Model selection is journaled before acknowledgement and restored on load/resume, including before the first prompt. Effort and mode remain connection-bound; generation changes apply at safe boundaries. Provider fallback does not become an operator selection. Use returned options after reattachment instead of assuming all picks persisted. |
| Turn settlement, E → C | Standard stop reasons describe model completion/cancellation. | `{"stopReason":"cancelled"}` is not used to hide a failed turn. Budget, permission, question, context overflow, and execution failures answer errors rather than invented success or model max-tokens. A pause uses `-32011`. Keep valid durable history and show recovery guidance; do not discard it as a model refusal. |
| Form elicitation, E → C request | Client advertises form support; accepts, declines, or cancels. | `{"action":"accept","content":{"answer":"choice-id"}}` is validated against the actual question. Single uses string `answer`; multiple uses string-array `answers`; form schemas must round-trip exactly through the pinned ACP schema. Unsupported/malformed answers fail, never grant authority. No form support produces unavailable question state; workspace trust remains untrusted. Display the explicit pending-question recovery rather than guess an answer. |
| Session time, E → C | `updatedAt` is standard RFC 3339 activity time; list order is not guaranteed. | Example `{"updatedAt":"2026-10-10T14:20:08.123Z"}` comes from the session's last durable event, not listing time. Sort by this field, not arrival order. Missing data must not be replaced with fabricated activity. |
| Usage, E → C | Standard `usage_update` with available usage/cost. | `{"sessionUpdate":"usage_update","used":1000,"size":20000}` uses the reporting generation's context window, including fallback models. Unknown monetary rates are not USD zero. Absent facts are omitted. Do not infer cost from token count or treat absent usage as zero. |

## Transport and deployment extensions

### WebSocket `/acp` and `EGO_ACP_TOKEN`

Direction: C → E HTTP admission, then bidirectional ACP. The current official
v1 transport page specifies stdio and permits custom transports; it does not
make ego's WebSocket path or bearer environment variable standard ACP fields.
ego uses the official HTTP adapter implementation for framing.

```http
GET /acp HTTP/1.1
Host: localhost:9000
Authorization: Bearer <EGO_ACP_TOKEN>
Connection: Upgrade
Upgrade: websocket
Sec-WebSocket-Version: 13
Sec-WebSocket-Key: <valid WebSocket key>
```

Network listeners require a non-empty `EGO_ACP_TOKEN` before serving, including
loopback TCP. Exactly one Authorization header must match the bearer bytes.
There is no newline JSON token preface and no ACP `authenticate` exchange for
this credential. Authentication happens before the ACP router factory.
Authenticated requests to another path get 404; non-GET/non-upgrade admission
gets 400; missing/wrong/duplicate credentials get 401. The adapter owns the
remaining WebSocket handshake validation. Local sockets use admitted OS peer
identity; stdio uses the launched process channel.

Fallback: if this transport is unavailable, launch `ego acp` over stdio or use
an admitted local socket. Do not try unauthenticated TCP or send the token in
ACP metadata. `--allow-public-bind` does not replace authentication.

### Listener resource bounds

Direction: deployment configuration controls incoming connections. These are
not JSON-RPC fields or negotiated capabilities. Example:

```sh
ego acp --listen 127.0.0.1:9000 --max-connections 8 --connection-lifetime 1h --max-ingress-bytes 16777216
```

Defaults are 32 simultaneous connections, 24-hour lifetime, and 128 MiB total
incoming bytes per connection. A permit lasts through the WebSocket upgrade.
The shared connection gate waits for capacity; lifetime and byte exhaustion
close the affected connection, not the listener. Zero bounds are invalid
configuration. No ACP error receipt is promised for transport exhaustion.
Fallback: reconnect, renegotiate, and reconcile session state using load/resume.
Do not automatically replay an uncertain effect or an accepted steer request.

## Not implemented

The current router excludes:

- Client filesystem `fs/read_text_file` and `fs/write_text_file`, and client
  `terminal/*` effects. ego executes through its own rooted tools instead.
- Audio formats other than WAV, mixed audio/image prompts, arbitrary binary
  resource media, and non-empty additional directories.
- `session/set_mode`: use the advertised mode config option.
- Dedicated model/provider configuration routes such as draft
  `providers/list`, `providers/set`, and `providers/disable`. They are not a
  way to redirect ego's provider base URL.
- Protocol-level `authenticate`/`logout` workflows. Transport bearer admission
  and provider login are separate surfaces; no auth methods are advertised here.
- Next-edit suggestions and document synchronization.
- URL elicitation and the newer schema's standard notice/compaction update
  variants. Current notices and compaction use the ego surfaces above. Client
  form support is the only elicitation capability captured by this adapter.

Trusted skill commands are advertised through standard `available_commands_update`
notifications and dispatched through the shared skill-command path. This does not
make every terminal slash control available to an ACP client.
- A v2 serving arm. v2 projection code and golden fixtures do not make v2
  reachable on the production router.
- Permission amendments that return replacement tool arguments. The ACP
  permission seat grants or denies; it does not implement core's amendment
  vocabulary.

Evidence: `crates/ego-acp/src/serve.rs:775` and the handler chain beginning at
`:2896`; `crates/ego-acp/src/negotiate.rs:49`;
`crates/ego-acp/src/controls.rs:1`, `crates/ego-acp/src/question.rs:32`, and
`crates/ego-acp/src/permission.rs:198`. Newer stable upstream schema entries
must not be mistaken for methods implemented by ego's older pinned SDK.

## Session initialization prompt

`session/new` accepts `_meta.ego.systemPrompt` and `_meta.ego.bare`:

```json
{"cwd":"/workspace/repo","mcpServers":[],"_meta":{"ego":{"systemPrompt":{"mode":"append","text":"Use concise answers"},"bare":true}}}
```

`systemPrompt` requires `mode` (`append` or `replace`) and string `text`.
Append extends the launch prompt; replace replaces it and its launch append.
`@file` resolves from the session workspace. Empty text is valid. Malformed
fields and unknown fields inside `systemPrompt` refuse creation. `bare` must
be boolean; a session cannot undo launch-wide bare mode. Bare and explicit
instructions conflict. Omitted values retain launch defaults.

These fields are implemented, not planned. They have no separate capability
advertisement. The stable prefix retains the selection across turns; creation
selections survive restart before the first prompt. Launch-wide `ego acp
--system TEXT --bare` is also implemented. See the
[session initialization guide](acp.md#session-initialization-prompt) for details.

### Session deletion

`sessionCapabilities.delete` advertises the standard `session/delete` method.
Its `_meta.ego.force: true` capability advertises explicit uncertain-run deletion:

```json
{"jsonrpc":"2.0","id":45,"method":"session/delete","params":{"sessionId":"S","_meta":{"ego":{"force":true}}}}
```

Omit `_meta.ego.force` (or use `false`) for normal deletion. It must be a
boolean. Success returns `{}`. Deletion is a durable tombstone: the target
leaves both CLI and ACP lists, but its history remains for descendant replay.
Other sessions are unchanged. Repeated deletion succeeds.

A live run or writer lease returns `-32010`; a held session returns `-32011`.
Neither can be overridden with force. Release a hold before deletion. An
uncertain run returns `-32012` unless force is explicitly true. Invalid IDs or
force types return `-32602`; missing sessions return `-32002`; store or
publication failures return `-32603`. Force acknowledges uncertainty; it does
not claim the external effect was resolved.

## Child activity (negotiated version 1)

The agent advertises `agentCapabilities._meta.ego.childActivity` with `version: 1`,
`update: "session_info_update"`, and `field: "_meta.ego.childActivity"`.
The client opts in on `initialize` with
`clientCapabilities._meta.ego.childActivity: {"version": 1}`.
Absent, malformed, and unknown versions receive no child updates. ACP protocol
version remains 1. This is metadata on the standard `session/update`, not a new
standard variant, custom notification method, tool call, or human wait.

Each `session/update.params.sessionId` is the exact parent session. The
`update.sessionUpdate` discriminator is `session_info_update`; its
`_meta.ego.childActivity` value has one of these shapes:

- Lifecycle: `{"version":1,"kind":"lifecycle","turnId":T,"itemId":I,"child":C,"phase":{"phase":"started"}}`.
- Settlement: the same identity with `phase: {"phase":"settled","outcome":"completed"}`.
  Outcomes are `completed`, `failed`, `interrupted`, or `cancelled`; cancellation is never success.
- Turn retraction: `{"version":1,"kind":"clear","turnId":T,"scope":"foreground"}`.
  Clear only foreground children of that turn, including omitted settlements.
  A durable child can outlive its parent turn, including parent cancellation;
  retain it until its exact settlement or session retraction. Do not invent an outcome.
- Session close/delete retraction: `{"version":1,"kind":"clear","turnId":null}`.
  Clear the whole session roster. Transport exit also retires its attachment.
- Load/resume snapshot: `{"version":1,"kind":"snapshot","children":[{"turnId":T,"itemId":I,"child":C}]}`.
  Resume includes the current durable child roster without message or usage
  history. Parent completion does not remove a still-active durable child.
  Replace the session roster, including when the array is empty. Historical
  starts and settlements are not delivered as new child activity.

`C` is the exact public `ChildIdentity`: `{"kind":"durable","invocation":UUID,"run":{"session_id":UUID,"nonce":UUID}}`
or `{"kind":"foreground","turn":UUID}`. No durable identity is invented for
foreground execution. Key a child by the parent session, parent turn, item, and
complete typed child identity; do not key it by a label. Duplicate starts upsert,
and settlements remove only their exact identity. Ignore a lifecycle for an
already cleared foreground turn. Durable children remain eligible for later settlement.
A child frame never grants permission or answers a question.
Private task text, results, diagnostic text, and human answers are absent.

Replay uses the durable lifecycle projection and excludes inherited children.
Foreground task lifecycle is live-only; a process restart cannot recover a
foreground identity that was never durable. A durable snapshot does not claim
that such a task survived the process. Existing permission and elicitation RPCs
remain their only human-request owners and are not replayed by this extension.
The extension does not replace ordinary ACP permission and question handling.
