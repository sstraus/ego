# ego extensions to ACP

This is the client integration contract audited on 2026-10-10. It describes the
code in this checkout, not a promise that every ACP client implements it.
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

There are **24 named extension entries**: 18 distinct keys directly inside
`_meta.ego`, four custom request methods, and two custom error codes. Nested
members and uses of the same key in different message types do not add entries.
The 18 keys are `steer`, `compact`, `hold`, `atMessage`, `trust`, `profile`,
`ceilingProfile`, `effective`, `warnings`, `atMessageId`, `summary`, `lineage`,
`inherited`, `providerRetry`, `salience`, `action`, `goalId`, and `goalState`.
Transport policy, permission choices, and standard-field interpretations are
listed separately below. There are no ego-named notification methods in the
router: extensions are carried by standard `session/update` notifications.

Source citations below are repository-relative `file:line` positions at the
audited checkout. Test citations identify existing source evidence. These tests
were read, not run, for this documentation change. JSON examples with `S`, `R`,
`T`, or `M` stand for valid non-nil session, request, target, or message UUIDs;
replace these labels before sending a request. Examples of metadata show the
containing standard object fragment, not a complete JSON-RPC envelope.

## Advertised capabilities

Source: `crates/ego-acp/src/serve.rs:775`. Test:
`crates/ego-acp/tests/session_lifecycle.rs::initialize_advertises_exactly_the_composed_v1_lifecycle_surface`.
The production durable composition advertises:

```json
{
  "loadSession": true,
  "promptCapabilities": {"image": true, "audio": false, "embeddedContext": false},
  "mcpCapabilities": {"http": true, "sse": false, "acp": true},
  "sessionCapabilities": {
    "list": {}, "delete": {"_meta": {"ego": {"force": true}}}, "resume": {}, "close": {},
    "fork": {"_meta": {"ego": {"atMessage": true}}}
  },
  "_meta": {"ego": {
    "steer": {"version": 1, "method": "_ego/steer", "contentTypes": ["text"]},
    "compact": {"version": 1, "method": "_ego/compact"},
    "hold": {"version": 1, "pause": "_ego/pause", "resume": "_ego/resume"}
  }}
}
```

`_meta.ego.steer`, `.compact`, `.hold`, and the fork capability's
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

All four custom requests are C → E, with an E → C JSON-RPC result. They require
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

Implementation: `crates/ego-acp/src/serve.rs:901`, `:3752`.
Tests: `crates/ego-cli/tests/acp_steer.rs::steer_during_tool_batch_reaches_next_provider_request`,
`idle_steer_returns_not_busy_without_starting_turn`, and
`cancel_after_steer_preserves_pending_text_in_durable_journal`.

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

Implementation: `crates/ego-acp/src/serve.rs:845`, `:881`, `:3811`;
publication shape: `crates/ego-core/src/session.rs:560`.
Tests: `crates/ego-acp/tests/compaction_extension.rs::compact_json_rpc_denies_unknown_request_fields_at_deserialization`
and `an_agent_without_the_compaction_port_neither_advertises_nor_serves_compact`.
The test `compaction_response_names_source_sequence_target_and_successor` uses
a hand-written JSON value; it is not a live receipt serialization test.
Successor authority is covered by
`crates/ego-cli/tests/acp_profile_ceiling.rs::acp_profile_ceiling_survives_fork_and_compaction_successors`.

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

Implementation: `crates/ego-acp/src/serve.rs:940`, `:977`, `:1756`, `:2556`.
Tests: `crates/ego-acp/tests/host_hold.rs::pausing_an_idle_session_records_the_boundary_and_answers_paused`,
`pausing_a_running_turn_answers_pending_and_not_paused`,
`a_resume_that_already_released_replays_its_answer_instead_of_refusing`,
`a_pause_redelivered_after_its_release_replays_rather_than_holding_again`, and
`a_hold_outlives_the_connection_that_asked_for_it`.

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

Implementation: `crates/ego-acp/src/serve.rs:2550`,
`crates/ego-acp/src/prompt_turn.rs:113`.
Tests: `crates/ego-acp/tests/host_hold.rs::a_refusal_names_a_missing_session_and_a_busy_one_as_what_they_are`
and `crates/ego-acp/tests/settlement.rs::held_session_is_discoverable_on_list_and_load_and_prompt_has_a_dedicated_code`.

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

Implementation: `crates/ego-acp/src/serve.rs:3070`,
`crates/ego-cli/src/acp.rs:1148`, `crates/ego-cli/src/acp_trust.rs:36`.
Tests: `crates/ego-cli/tests/acp_host_trust.rs::selected_profile_attestation_loads_instructions_without_elicitation`
and `unprofiled_attestation_is_ignored_and_the_form_is_used`.

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

Implementation: `crates/ego-acp/src/serve.rs:2146`, `:2983`, `:2041`;
`crates/ego-cli/src/acp_confinement.rs:106`, `:145`.
Tests: `crates/ego-cli/tests/acp_profile_ceiling.rs::acp_profile_ceiling_clamps_with_warnings_and_keeps_narrower_roots`,
`acp_profile_ceiling_rejects_bad_names_and_preserves_absent_ceiling`,
`acp_profile_ceiling_survives_load_and_resume_on_fresh_connections`, and
`acp_profile_ceiling_missing_on_reopen_refuses_instead_of_widening`.

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

Implementation: `crates/ego-acp/src/serve.rs:796`, `:3535`.
Tests: `crates/ego-acp/tests/session_lifecycle.rs::message_fork_replays_the_cutoff_before_local_history_even_after_parent_deletion`
and `invalid_message_forks_publish_no_child`.

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

Implementation: `crates/ego-acp/src/serve.rs:2615`, `:2640`, `:3327`.
Tests: `crates/ego-acp/tests/settlement.rs::held_session_is_discoverable_on_list_and_load_and_prompt_has_a_dedicated_code`,
`crates/ego-cli/tests/acp_synopsis.rs::first_idle_publishes_a_durable_title_and_bounded_summary`,
and `crates/ego-acp/tests/session_lifecycle.rs::compaction_children_project_lineage_in_list_and_load_info`.

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

Implementation: `crates/ego-acp/src/project.rs:570`.
Test: `crates/ego-acp/tests/session_lifecycle.rs::critic_nested_fork_keeps_each_cutoff_and_local_ids_after_ancestor_deletion`.

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

Implementation: `crates/ego-acp/src/project.rs:242`, `:268`;
`crates/ego-acp/src/sink.rs:89`, `:130`.
Test: `crates/ego-acp/tests/provider_retry_projection.rs::a_retry_is_a_session_info_patch_with_the_counter_the_wait_and_the_cause`.
This test covers projection and null clearing, not a live provider outage.

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

Implementation: `crates/ego-acp/src/project.rs:211`, `:219`, `:281`;
`crates/ego-acp/src/serve.rs:207`.
Tests: `crates/ego-acp/tests/notice_projection.rs::card_notice_is_a_standard_message_with_an_open_result_action`,
`crates/ego-acp/tests/tool_projection.rs::tool_activity_is_marked_on_creation_and_progress_without_changing_acp_fields`,
`reasoning_and_failed_tool_completion_remain_collapsible_activity`, and
`crates/ego-acp/tests/session_lifecycle.rs::journal_failure_does_not_report_success_after_the_notice_was_queued`.
The answer/approve action serialization variants are **untested** by the named
notice projection test.

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

Implementation: `crates/ego-acp/src/project.rs:811`;
`crates/ego-acp/src/serve.rs:2678`.
Tests: `crates/ego-acp/tests/session_lifecycle.rs::loading_a_goal_emits_one_current_plan_entry`
and the goal projection cases in `crates/ego-acp/tests/acp_golden.rs::live_turn_boundaries_project_the_current_goal_status_without_inventing_steps`.

## Standard vs ego

This table includes standard-field interpretations and deployment policies.
They are not extra `_meta.ego` keys. Fallback means a safe client action, not a
second protocol implementation.

| Surface and direction | Standard baseline | ego shape, semantics, errors, and fallback | Source and existing test |
| --- | --- | --- | --- |
| `initialize`, C → E → C | Negotiate a supported protocol version; report capabilities. | `{"protocolVersion":1}` for supported asks at or above 1. Version 0 is refused by the SDK. Do not assume that compiled v2 projections are served. Use the returned version or refuse the connection. | `crates/ego-acp/src/negotiate.rs:49`; `initialize.rs::the_router_answers_what_negotiate_says_it_answers` |
| `cwd`, C → E on session setup | Absolute path required by the compared schema. | Production also accepts a relative path, for example `{"cwd":"repo","mcpServers":[]}`, relative to the launch base. It canonicalizes and requires an existing directory. Load/resume require the durable session root to match. Bad roots are invalid-params. Portable clients must send an absolute path. This is a permissive difference, not an advertised extension. | `crates/ego-cli/src/host_ports.rs:55`; `host_ports.rs::one_resolver_answers_both_front_doors_and_names_the_right_thing`; `session_lifecycle.rs::malformed_unknown_and_wrong_root_requests_return_explicit_errors` |
| `additionalDirectories`, C → E | Optional extra roots, subject to capability negotiation. | Any non-empty array, such as `{"additionalDirectories":["/other"]}`, is invalid-params before root resolution or MCP effects, for new/load/resume/fork. Omit it or send `[]`; it is not a way to loosen the profile perimeter. | `crates/ego-acp/src/serve.rs:2964`, `:3212`, `:3369`, `:3510`; `session_lifecycle.rs::every_nonempty_additional_directories_field_refuses_before_other_effects` |
| `mcpServers`, C → E | Client supplies MCP server descriptions. HTTP/SSE are capability-gated. | Example stdio entry: `{"name":"tools","command":"/usr/bin/tool-server","args":[],"env":[]}`. ego requires absolute stdio commands, valid case-insensitively unique names/env names, valid HTTP URLs and headers, no duplicate or wire-owned HTTP headers, and non-empty ACP server IDs. Plain HTTP is limited to exact loopback. SSE/unknown transports refuse; the raw-versus-parsed count check prevents silently dropped servers. Invalid selection is refused atomically. Omit unsupported servers rather than assume partial admission. | `crates/ego-acp/src/serve.rs:1426`, `:2196`; `prompt.rs::invalid_supplied_mcp_servers_are_rejected_atomically_before_session_insert`, `session_new_accepts_exact_loopback_http_and_refuses_other_plaintext_hosts` |
| `session/prompt` content, C → E | Text and resource links are baseline; images are advertised. | `[{"type":"resource_link","uri":"file:///workspace/a","name":"a"}]` becomes URI text, not an automatic file read. Text joins with newlines. Images are separate from text; interleaving is lost. JPEG/PNG/GIF/WebP require valid base64 and matching decoded media. Audio/embedded resources refuse before a turn. Model limits still apply. Send text-only if image support is absent. | `crates/ego-acp/src/serve.rs:1686`, `:1718`; `prompt.rs::the_two_block_kinds_every_agent_must_accept_reach_the_turn`, `an_image_whose_bytes_contradict_its_declared_type_is_refused` |
| Prompt admission, C → E | One prompt request settles its turn. | A second simultaneous prompt is invalid-request, not a queue. Unopened sessions are invalid-params. Correlation uses request content digest as well as identity; changed content is new work. Use advertised steering for busy input, otherwise wait. | `crates/ego-acp/src/prompt_turn.rs:79`, `:96`; `prompt.rs::a_second_prompt_while_a_turn_is_running_is_refused_and_starts_nothing`, `session_lifecycle.rs::a_reused_request_id_carrying_a_new_prompt_is_new_work_not_a_replay` |
| Load/resume, C → E → C | Load replays history; resume does not. | `{"sessionId":"S","cwd":"/workspace/repo","mcpServers":[]}` reattaches durable execution and restores creation profile authority. Load replays ordered projectable history and at most one final reduced usage update; resume emits current info but no history/usage replay. Same-connection active replacement refuses. Uncertain effects require close/new, not replay. With no resume capability, use advertised load; never use standard resume to release a hold. | `crates/ego-acp/src/serve.rs:3197`, `:3354`; `session_lifecycle.rs::load_replays_projectable_history_then_one_final_usage_and_current_info`, `resume_emits_current_info_but_no_history_or_usage`; `uncertain_session.rs::uncertain_session_prompt_names_run_and_close_new_chat_recovery` |
| Permission options, E → C request, C → E answer | Option IDs are opaque; option kind indicates allow/reject once/always. | `{"outcome":{"outcome":"selected","optionId":"allow-session"}}` grants transient session approval, despite its standard kind `allow_always`. IDs `allow-once`, `reject-once`, `allow-session`, and when rememberable `allow-always`/`reject-always` are distinct. Remembered answers write displayed user rules in the active profile. Cancellation, unknown IDs, no seat, transport failure, or persistence failure deny. No timeout is imposed on a legitimate open permission question. Render offered labels and return the exact ID; never infer persistence from kind alone. Fallback is once-only choices when that is all the agent offers, not terminal input. | `crates/ego-acp/src/permission.rs:57`, `:121`, `:235`; `permission.rs::an_option_ego_never_offered_is_a_denial`; `crates/ego-cli/tests/acp_remembered_permissions.rs::session_allow_answers_five_later_calls_without_a_prompt_then_expires`, `allow_always_writes_one_profile_rule_and_revoke_restores_the_question` |
| Sandbox path permissions, E → C request, C → E answer | Standard permission response envelope. | `{"outcome":{"outcome":"selected","optionId":"sandbox-session"}}` permits the shown path for this attachment. `sandbox-always` persists the workspace grant; `sandbox-deny` refuses. The question shows the command that will run again. Transport/unknown answer/persistence failures deny. Ordinary tool approval does not imply a sandbox path grant. Render the offered choices; do not synthesize an unoffered grant. | `crates/ego-acp/src/sandbox.rs:47`, `:73`, `:123`; `crates/ego-cli/tests/acp_sandbox_grants.rs::session_answer_runs_the_command_again_and_is_not_asked_twice_but_leaves_no_stored_grant`, `denied_answer_tells_the_model_the_sandbox_refused_and_does_not_run_the_command_again` |
| Config options, E → C offer, C → E selection | Standard select controls and optional categories. | `{"sessionId":"S","configId":"sandbox","value":"workspace"}` selects only a currently offered value. IDs are `model`, `effort`, `mode`, `sandbox`; sandbox uses category `sandbox`, not mode. `__ego_unselected__` is an offered no-op only while model/effort is unset. Boolean, unoffered values, and unknown IDs refuse. Absent seat is method-not-found. Full replacement options return and publish after acceptance; mode also publishes current-mode update. Sandbox can narrow but not exceed the ceiling and cannot change after stdio MCP pins it. Read the offer; do not hard-code choices or send `session/set_mode`. | `crates/ego-acp/src/controls.rs:30`, `:95`; `crates/ego-acp/src/serve.rs:3908`; `controls_router.rs::an_unoffered_value_is_refused_before_the_seat_is_asked`, `an_opaque_unselected_answer_is_a_wire_noop_without_staging`; `crates/ego-cli/tests/acp_mcp_scope_critic.rs::sandbox_pick_after_stdio_admission_is_refused_but_free_without_servers` |
| Durable model selection, C → E → C | Config selection changes the session model. | Model selection is journaled before acknowledgement and restored on load/resume, including before the first prompt. Effort and mode remain connection-bound; generation changes apply at safe boundaries. Provider fallback does not become an operator selection. Use returned options after reattachment instead of assuming all picks persisted. | `crates/ego-cli/src/acp_controls.rs:325`, `:545`; `crates/ego-cli/tests/acp_model_load.rs::selected_model_survives_load_before_the_first_prompt`, `resumed_session_sends_and_meters_the_selected_model` |
| Turn settlement, E → C | Standard stop reasons describe model completion/cancellation. | `{"stopReason":"cancelled"}` is not used to hide a failed turn. Budget, permission, question, context overflow, and execution failures answer errors rather than invented success or model max-tokens. A pause uses `-32011`. Keep valid durable history and show recovery guidance; do not discard it as a model refusal. | `crates/ego-acp/src/project.rs:422`, `crates/ego-acp/src/prompt_turn.rs:388`; `prompt.rs::an_integrity_failure_answers_an_error_and_not_a_cancelled_stop`, `settlement.rs::reject_permission_without_cancel_remains_a_single_refusal` |
| Form elicitation, E → C request | Client advertises form support; accepts, declines, or cancels. | `{"action":"accept","content":{"answer":"choice-id"}}` is validated against the actual question. Single uses string `answer`; multiple uses string-array `answers`; form schemas must round-trip exactly through the pinned ACP schema. Unsupported/malformed answers fail, never grant authority. No form support produces unavailable question state; workspace trust remains untrusted. Display the explicit pending-question recovery rather than guess an answer. | `crates/ego-acp/src/question.rs:32`, `:161`, `:298`; `question.rs::accepted_content_is_revalidated_by_core_before_settlement`, `question_live.rs`; `trust.rs::a_host_without_form_elicitation_is_never_asked_and_stays_untrusted` |
| Session time, E → C | `updatedAt` is standard RFC 3339 activity time; list order is not guaranteed. | Example `{"updatedAt":"2026-10-10T14:20:08.123Z"}` comes from the session's last durable event, not listing time. Sort by this field, not arrival order. Missing data must not be replaced with fabricated activity. | `crates/ego-acp/src/serve.rs:2640`; `session_lifecycle.rs::a_listed_session_states_its_last_activity_as_rfc_3339` |
| Usage, E → C | Standard `usage_update` with available usage/cost. | `{"sessionUpdate":"usage_update","used":1000,"size":20000}` uses the reporting generation's context window, including fallback models. Unknown monetary rates are not USD zero. Absent facts are omitted. Do not infer cost from token count or treat absent usage as zero. | `crates/ego-acp/src/project.rs:534`; usage projection cases in `crates/ego-acp/tests/acp_golden.rs`; `session_lifecycle.rs::load_replays_projectable_history_then_one_final_usage_and_current_info` |

Test file names without a crate prefix in the table refer to
`crates/ego-acp/tests/`.

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

Implementation: `crates/ego-acp/src/auth.rs:9`, `:42`;
`crates/ego-acp/src/serve.rs:273`, `:290`, `:353`.
Tests: `crates/ego-acp/tests/auth.rs::empty_acp_secret_is_refused_and_debug_is_redacted`,
`crates/ego-acp/tests/listener_transport.rs::tcp_authentication_rejects_missing_wrong_and_non_websocket_requests_before_factory`
and `authenticated_websocket_initialize_uses_the_sdk_client`.

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

Implementation: `crates/ego-listen/src/limits.rs:13`, `:126`, `:200`, `:221`;
`crates/ego-acp/src/serve.rs:408`.
Tests: `crates/ego-acp/tests/listener_transport.rs::authenticated_websocket_factory_is_fresh_and_the_permit_survives_http_101`,
`connection_lifetime_closes_one_connection_but_the_listener_survives`, and
`ingress_shutdown_closes_only_the_exhausted_connection`.

## Not implemented

The router and [current product contract](../internals/06-acp.md) exclude:

- Client filesystem `fs/read_text_file` and `fs/write_text_file`, and client
  `terminal/*` effects. ego executes through its own rooted tools instead.
- Audio and embedded context, MCP-over-SSE, and non-empty additional directories.
- `session/set_mode`: use the advertised mode config option.
- Dedicated model/provider configuration routes such as draft
  `providers/list`, `providers/set`, and `providers/disable`. They are not a
  way to redirect ego's provider base URL.
- Protocol-level `authenticate`/`logout` workflows. Transport bearer admission
  and provider login are separate surfaces; no auth methods are advertised here.
- Next-edit suggestions and document synchronization.
- Standard slash-command advertisement/dispatch, URL elicitation, and the newer
  schema's standard notice/compaction update variants. Current notices and
  compaction use the ego surfaces above. Client form support is the only
  elicitation capability captured by this adapter.
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

## Planned, not implemented: regression case

Pending story **regression_case**, requested on 2026-10-10, specifies:

- C → E `session/new` `_meta.ego.systemPrompt`, with append or replace mode.
- C → E `session/new` `_meta.ego.bare`, with CLI-equivalent validation,
  including the bare/instructions conflict.
- Launch-wide ACP `--system` and `--bare` parity, and a stable system prefix
  across turns.

These keys are **not current extensions** and are not included in the count.
The story does not yet establish a complete JSON wire shape, error mapping,
capability advertisement, or implemented test. Do not send guessed fields and
assume they changed the prompt: the current session metadata parser reads
profile names and trust, not these planned keys. Current launch-wide
`--append-system` is separate from a per-session override. Until the story is
implemented, use the existing launch configuration and separate processes when
sessions require different launch-time instructions.

Source evidence: regression case read with `wiz-run stories-cli.js show regression_case`
from the main checkout; current parser at `crates/ego-acp/src/serve.rs:2146` and
trust reader at `:3070`. Planned implementation and tests: **untested / absent**.

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
