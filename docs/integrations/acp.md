# ACP and TUICommander

ACP lets a host own the conversation UI while εgo owns durable execution, policy,
and evidence. TUICommander is one host; an ACP connection itself is not proof of
full compatibility with every host capability.

For an installation-free custom-agent setup and editor context recipe, see
[IDE setup over native ACP](ide.md).

For wire shapes, fallback rules, source citations, and capability boundaries, see
[ego extensions to ACP](acp-extensions.md).
For pinned standard TypeScript, Python, and Rust libraries and runnable clients,
see [Drive ego with standard ACP client libraries](acp-clients.md).

## Launch and transport

For stdio, the host launches executable `ego` with argument `acp`. Protocol stdout
is reserved for ACP; diagnostics stay off that stream. `--listen ENDPOINT` selects
a local socket or an admitted network endpoint. Local sockets authenticate through
OS peer identity; network ACP uses the implemented official WebSocket adapter and
transport authentication (`EGO_ACP_TOKEN`) at `/acp`, not a custom token preface
on newline JSON-RPC.

Launch options include `-C`, `--session-dir`, `--profile`, `--instructions`,
`--system`, `--append-system`, `--bare`, `--tools`/`--no-tools`, and `--allow`/`--deny`. `--sandbox` is
the weakest confinement a session may choose, not an unconditional host grant.
`--allow-public-bind` alone does not establish network authentication.

ACP listener defaults are 32 simultaneous connections, 24-hour connection
lifetime, and 128 MiB cumulative ingress per connection. `--max-connections`,
`--connection-lifetime`, and `--max-ingress-bytes` tighten these explicit bounds.
Source: [listener limits](../../crates/ego-listen/src/limits.rs).

## Headless multi-turn stdio

ACP is the only headless machine conversation protocol. Launch `ego acp` on
macOS or Linux and keep its stdin and stdout open across turns. There is no
separate `--input-format stream-json` mode. `ego run --output jsonl` emits events
for a one-shot run; those events are not ACP input frames.

Stdio uses UTF-8 JSON-RPC 2.0, one complete JSON object per newline, with no
`Content-Length` header. Flush each outgoing frame. Read stdout continuously,
even while a prompt response is pending: the agent streams `session/update`
notifications and can issue requests that need a host answer before it can
continue. Keep stderr separate from protocol stdout.

The following is an illustrative ACP v1 exchange, not a recorded provider run.
`SESSION`, `CALL`, and `PERMISSION_ID` stand for runtime IDs. Initialization and
session creation results include other negotiated capabilities and controls
omitted here; clients must read the actual responses.

Host → agent: initialize, then wait for its response before creating a session.

```jsonl
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
```

Agent → host: the result selects `protocolVersion: 1` and advertises capabilities.
Host → agent, after that response:

```jsonl
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/workspace/repo","mcpServers":[]}}
```

Agent → host: use `result.sessionId` from response 2 for both prompts below.
The first prompt includes an attachment with the host's text content, not a
request for ego to fetch a URI.

Host → agent:

```jsonl
{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"SESSION","prompt":[{"type":"text","text":"Summarize these notes"},{"type":"resource","resource":{"uri":"untitled:notes","mimeType":"text/plain","text":"The build is offline."}}]}}
```

Agent → host, after any streamed updates:

```jsonl
{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}
```

Host → agent: a second turn in the same session.

```jsonl
{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"SESSION","prompt":[{"type":"text","text":"Check the notes against the workspace"}]}}
```

If policy requires approval, the agent → host request has this shape. The
actual title, call ID, request ID, and offered options come from the live request;
this example shows only the one-use choices.

```jsonl
{"jsonrpc":"2.0","id":"PERMISSION_ID","method":"session/request_permission","params":{"sessionId":"SESSION","toolCall":{"toolCallId":"CALL","title":"Read workspace notes"},"options":[{"optionId":"allow-once","name":"Allow once","kind":"allow_once"},{"optionId":"reject-once","name":"Reject once","kind":"reject_once"}]}}
```

Host → agent: answer that exact agent-originated ID, preserving its JSON type.
Only send this response when such a request is pending; select an option it
actually offered. A declined host dialog can instead answer with
`{"outcome":{"outcome":"cancelled"}}` as the result.

```jsonl
{"jsonrpc":"2.0","id":"PERMISSION_ID","result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}
```

Host → agent, while prompt 4 is still pending: cancel the current turn.

```jsonl
{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"SESSION"}}
```

Cancellation has no request ID and gets no separate response. The pending prompt
settles after cancellation reaches the run; a turn that already completed can
still have returned `end_turn`. For a cancelled run, agent → host:

```jsonl
{"jsonrpc":"2.0","id":4,"result":{"stopReason":"cancelled"}}
```

### Attachments, flow control, and shutdown

- Text attachments use `resource` blocks as above. `resource_link` carries a
  named URI reference, not the referenced bytes. Images use `image` blocks with
  `mimeType` and base64 `data`; consult negotiated prompt capabilities and model
  support. Unsupported content and malformed parameters are refused rather than
  silently omitted. When audio is advertised, `audio` blocks accept explicit
  `audio/wav` with valid base64 WAV bytes, at most four clips and 10 MiB total.
  Mixed audio/image prompts are refused; the selected model must support audio.
  Existing attachment fixtures are in
  [prompt tests](../../crates/ego-acp/tests/prompt.rs).
- Keep one prompt in flight per session. Wait for its response before the next
  ordinary prompt; do not treat write completion as turn completion. Dispatch
  cancellation and permission responses independently while a prompt runs.
- Bound the host's own outgoing queue and respect pipe write backpressure.
  Drain stdout concurrently to avoid a full pipe preventing an approval request
  or prompt response from reaching the host. Do not copy listener-only limits
  onto stdio or assume an unbounded prompt queue is supported.
- Invalid prompt parameters return a JSON-RPC error. Match responses by ID and
  inspect `error` rather than assuming every response has `result`. Do not
  replay a prompt blindly after a transport error: the work might have started.
- An unavailable durable session returns `ResourceNotFound`. For a missing or
  quarantined lookup, `error.message` names the session ID and filesystem store
  root, and `error.data` is an object with `sessionId` and `storeRoot`. The ID is
  the failed lookup, which can be a catalogue candidate or ancestor rather than
  the selected chat. `storeRoot` is the absolute session-storage path, not the
  workspace cwd; a non-filesystem store reports `null`. Keep these diagnostics
  when reporting a failure. Do not recreate the session implicitly or treat an
  optional catalogue failure as proof that another attached chat is missing.
- EOF closes the ACP connection; it does not submit another prompt. For an
  orderly shutdown, cancel pending work when needed, answer pending host
  requests, drain the prompt settlement, then close stdin and reap the process.
  Do not use EOF as a substitute for a correlated cancellation response.

The offline [headless stdio tests](../../crates/ego-cli/tests/acp_headless_stream.rs)
launch the real `ego acp` process against the existing local provider fixture:
multiple turns, embedded text, malformed parameters, and EOF. The existing
[settlement tests](../../crates/ego-cli/tests/acp_settlement.rs) cover permission
answers, cancellation, and another prompt after a cancelled streaming turn.
These fixtures use no paid calls and do not prove live host compatibility.
The CLI's stdio composition uses the SDK `Stdio` transport in
[acp.rs](../../crates/ego-cli/src/acp.rs).

## Sessions, permissions, and controls

ACP sessions map to the durable core. Host sessions can load/list/resume/fork and
use supported lifecycle controls advertised by this build. A host's trust answer
belongs to its session. Permission UI maps prepared-call approval onto current
session/user rule authority; unsupported or malformed replies refuse with evidence.
Session-wide allowance is transient and still cannot raise system ceilings.

Model, effort, mode, confinement, queue/steering, pause/resume, and compaction are
capability-negotiated controls. εgo extensions are versioned under `_ego`; read
what initialization advertises rather than guessing from a product version.
`ego doctor --json` reports the local ACP surface without opening a hosted run.

The `_ego/compact` extension identifies the successor and makes it available to
the next standard session prompt. Standard tip forks and εgo's CLI completed-message
fork selector have different surfaces; do not assume all CLI flags are ACP fields.
Subscription usage is reported without pretending that an unknown monetary rate
is USD 0.

## Prompt settlement and recovery

The response to `session/prompt`, not a streamed text chunk or a successful pipe
write, is the turn settlement. A completed run returns `end_turn`; cancellation
returns `cancelled`. A known execution/provider failure or uncertain result
returns a correlated JSON-RPC error rather than pretending the host cancelled.
A held or newly paused prompt uses error code `-32011`; resume it through the
advertised `_ego/resume` control. Context overflow reports an error with
`_ego/compact` guidance, not a fabricated normal completion.

Source owners: [prompt dispatch](../../crates/ego-acp/src/prompt_turn.rs),
[durable settlement mapping](../../crates/ego-acp/src/serve.rs), and
[ACP stop-reason projection](../../crates/ego-acp/src/project.rs).
[Packaged stdio tests](../../crates/ego-cli/tests/acp_settlement.rs) cover
cancellation during a permission wait or stream, durable holds after reconnect,
a known HTTP 400 followed by another prompt, and restart after a killed
generation. Use live integration checks for host-specific behavior.

## Host verification boundary

Validate a host integration through its actual protocol connection and backend.
Deterministic adapter tests do not establish every host's behavior or UI rendering.

Sources: [ACP composition](../../crates/ego-cli/src/acp.rs),
[ACP adapter crate](../../crates/ego-acp/src/),
[permission adapter](../../crates/ego-cli/src/acp_permission.rs), and
[host tests](../../crates/ego-cli/tests/tuicommander_host.rs).

## Session profile ceiling

A host can select a session profile in `session/new` and bound it by its machine
profile. Names resolve through the existing ego config layers; this does not
import `.tuic.json`. The host passes the names from that file and its machine
selection:

```json
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/workspace/repo","mcpServers":[],"_meta":{"ego":{"profile":"repo","ceilingProfile":"machine"}}}}
```

`profile` overrides the launch profile for this session. `ceilingProfile` is
optional and resolves from machine layers (system, user, environment), even if
the workspace is trusted. A repository cannot redefine the ceiling profile.
Omitting both fields preserves the existing launch behavior. Omitting only the
ceiling selects the requested profile without adding a host restriction.
Malformed names and unknown profiles refuse creation.

The effective mode and sandbox retain the narrower value. Extra roots are
compared after canonicalization using the system root-ceiling rule: roots
outside the machine perimeter are dropped; write access above a read-only
bound becomes read-only. Additional executable read roots are also bounded.
The workspace remains admitted. Administrator ceilings in `/etc/ego/config.toml`
still apply above the host clamp and retain their existing refusal semantics.

A successful result includes `_meta.ego` with the selected names, effective
values and warnings. For a repository profile with `mode="yolo"`, `sandbox="off"`
and an extra root outside a workspace-only machine profile:

```json
{"sessionId":"<uuid>","_meta":{"ego":{"profile":"repo","ceilingProfile":"machine","effective":{"mode":"default","sandbox":"workspace","roots":[],"execPaths":[]},"warnings":["mode yolo exceeds profile ceiling default; using default","sandbox off exceeds profile ceiling workspace; using workspace","root /outside exceeds profile ceiling; dropped"]}}}
```

Warnings are strings in request-field order (mode, sandbox, roots, executable
roots); an unchanged request has an empty array. Creation succeeds after a host
clamp. Later controls cannot exceed the admitted ceiling, and prompt preparation
carries both profile names in durable run selections. Deferred confinement
measurement still publishes the standard `config_option_update` after creation.

Creation writes the profile names into the durable session log header, before
the first prompt. The manifest projects these names from that header.
`session/load` and `session/resume` restore this creation authority and durable
run selections before measuring confinement or admitting MCP servers. Forks and
compacted successors inherit the same names through durable session lineage,
including successors with no run records yet. A missing or invalid ceiling
refuses reopening; it never becomes an absent ceiling. Reattaching to a live
profile seat also refuses changed profile policy instead of replacing its
pinned authority.

## Session initialization prompt

`ego acp --system "Use concise answers" --bare` sets launch defaults. As in
`ego run`, `--system` replaces workspace instructions, `--append-system` appends
to the selected prompt, and `--bare` disables ambient discovery. `--bare` with
`--instructions` is refused. Explicit prompt text still works in bare mode.
The agent identity and runtime facts retain the existing CLI behavior.

A host can select a prompt for one `session/new` request:

```json
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/workspace/repo","mcpServers":[],"_meta":{"ego":{"systemPrompt":{"mode":"append","text":"Use concise answers"},"bare":true}}}}
```

`systemPrompt` requires `mode` (`append` or `replace`) and string `text`.
Append extends the launch prompt, including its launch-wide append text.
Replace substitutes the session text for the launch prompt and its append text.
Both modes accept `@file`, resolved against the session workspace, as CLI flags
do. An empty string is valid. Invalid modes, types, missing fields, and unknown
fields inside `systemPrompt` refuse creation. `bare` must be a boolean.

Omitted fields preserve launch defaults. A session can enable bare mode but
cannot undo launch-wide `--bare` with `bare:false`. A session bare selection
also conflicts with launch-wide `--instructions`. These selections belong only
to the created session and remain in its stable system prefix across turns;
they do not become user messages. Run selections retain them for a later load
or fork after the session has started its run.
