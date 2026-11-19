# Drive ego with standard ACP client libraries

Use an existing ACP client library to launch `ego acp` and drive its durable
sessions. Do not build an ego SDK or another RPC protocol. These examples use
stdio and ACP **protocol version 1**, not the experimental v2 entry points.
SDK package versions are not protocol versions.

For launch options and the raw stdio contract, see [CLI.md](../../CLI.md).
For ego's extension payloads, capability advertisements, and fallback rules, see
[ego extensions to ACP](acp-extensions.md). This guide covers library use; it
does not duplicate the raw JSON-RPC transcript.

## Pinned library versions (2026-10-16)

Use the official ACP organization library in each language. All three
repositories were public, not archived, and licensed **Apache-2.0** at this
audit. The dates below are the GitHub repository `pushed_at` values observed on
that date, not a support commitment. The package registries returned the stated
versions as their latest releases. Pin them: current examples and experimental
APIs can change independently of ego.

| Language | Recommended package and exact version | Audited source | Last repository push observed |
| --- | --- | --- | --- |
| TypeScript | [`@agentclientprotocol/sdk@1.9.0`](https://registry.npmjs.org/@agentclientprotocol/sdk/1.9.0) | [`typescript-sdk` at `1db4d43286d993545e729376346b7f9098748159`](https://github.com/agentclientprotocol/typescript-sdk/tree/1db4d43286d993545e729376346b7f9098748159) | 2026-10-16 |
| Python | [`agent-client-protocol==0.12.1`](https://pypi.org/project/agent-client-protocol/0.12.1/) (import `acp`) | [`python-sdk` at `bdb9ac7054fb2df76a922b60b356c3c65f8a8a64`](https://github.com/agentclientprotocol/python-sdk/tree/bdb9ac7054fb2df76a922b60b356c3c65f8a8a64) | 2026-10-13 |
| Rust | [`agent-client-protocol = "=3.4.0"`](https://crates.io/crates/agent-client-protocol/3.4.0) | [`rust-sdk` at `d5e8bf4d17eafed81b2185acd4bd366e88a33dd1`](https://github.com/agentclientprotocol/rust-sdk/tree/d5e8bf4d17eafed81b2185acd4bd366e88a33dd1) | 2026-10-16 |

Each has recent source activity and published releases. This is evidence of
maintenance at the audit date, not proof that every draft feature interoperates.
No third-party wrapper is needed for the extension cases below. Keep a lockfile
in a production client to pin transitive dependencies as well.

### Extension and permission seams

| Concern | TypeScript 1.9.0 | Python 0.12.1 | Rust 3.4.0 |
| --- | --- | --- | --- |
| Connect and receive updates | `client(...).onNotification(...).connectWith(ndJsonStream(...), ...)` | Subclass `Client`; `connect_to_agent` drives `session_update` callbacks | `Client.builder().on_receive_notification(...).connect_with(AcpAgent, ...)` |
| Send ego extension | `ctx.request("_ego/pause", params)`; the full wire name is unchanged | `conn.ext_method("ego/pause", params)`; the SDK **prepends `_`** | `send_request(UntypedMessage::new("_ego/pause", params)?)`; or implement `JsonRpcMessage` and `JsonRpcRequest` for an application-owned typed payload |
| Attach custom metadata | `_meta: { ego: {...} }` in a standard request | `ego={...}` keyword extras on generated helpers become `_meta.ego`; received metadata is `field_meta` | Standard request's `meta` field serializes as `_meta` |
| Answer permissions | Register `session/request_permission` with `onRequest`; reply with `outcome` | Implement `request_permission`; return `RequestPermissionResponse` | Register `RequestPermissionRequest` with `on_receive_request`; respond through its responder |
| Resume and fork | `ctx.request("session/resume", ...)`, generic `ctx.request("session/fork", ...)` | `resume_session`, `fork_session` | `ResumeSessionRequest`, `ForkSessionRequest`; fork needs `unstable_session_fork` |

Important API differences:

- TypeScript's generic `request`/`notify` methods accept custom method names and
  payload types. `ClientSideConnection` and its `extMethod` helper remain present
  but are deprecated at this pin. The example uses the current builder API.
- Python's `ext_method("_ego/pause", ...)` sends **`__ego/pause`**, which is wrong.
  Pass `ego/pause`. Standard helpers use snake_case argument names; the SDK
  serializes camelCase wire fields. Construct `TextContentBlock(type="text",
  text="...")` explicitly; the discriminator is required at this pin.
- Rust 3.4.0 uses the connection builder and `ConnectionTo<Agent>` API. Do not
  copy older examples written for ego's own pinned 2.0.0 dependency. A client
  dependency need not match ego's internal dependency version: the shared wire
  contract is ACP v1. The `process` feature enables `AcpAgent` child transport.
- `session/load` replays history; `session/resume` attaches without that replay.
  `session/fork` is an upstream **draft** surface, not an ego-named extension.
  Check `agentCapabilities.sessionCapabilities` before resume or fork. Check
  `loadSession` before load. An empty capability object means supported, not
  false. Do not select the v2 protocol just to get a fork helper.
- An ordinary client can ignore `_meta.ego`. An extension-aware client checks
  the advertised extension version before sending its exact payload. Unknown
  methods fail; metadata does not grant permission or bypass confinement.

Primary API sources at the audited revisions:
[TypeScript connection and builder](https://github.com/agentclientprotocol/typescript-sdk/blob/1db4d43286d993545e729376346b7f9098748159/src/acp.ts),
[Python client connection](https://github.com/agentclientprotocol/python-sdk/blob/bdb9ac7054fb2df76a922b60b356c3c65f8a8a64/src/acp/client/connection.py),
[Rust process transport](https://github.com/agentclientprotocol/rust-sdk/blob/d5e8bf4d17eafed81b2185acd4bd366e88a33dd1/src/agent-client-protocol/src/acp_agent.rs),
[Rust generic messages](https://github.com/agentclientprotocol/rust-sdk/blob/d5e8bf4d17eafed81b2185acd4bd366e88a33dd1/src/agent-client-protocol/src/jsonrpc.rs),
and [Rust one-shot client](https://github.com/agentclientprotocol/rust-sdk/blob/d5e8bf4d17eafed81b2185acd4bd366e88a33dd1/src/agent-client-protocol/examples/yolo_one_shot_client.rs).
The example here deliberately does **not** copy that last example's first-option
automatic approval policy.

## Runnable clients

Sources:
[TypeScript](examples/acp/client.ts), [Python](examples/acp/client.py),
[Rust](examples/acp/rust/src/main.rs), and the shared
[isolated launcher](examples/acp/isolated.sh).
These are standalone consumer examples, not new ego workspace packages.

Each client initializes, creates a session with `_meta.ego.bare`, registers
streaming and permission handlers, calls the advertised ego pause/resume
extension, submits a prompt, sends cancel, loads history, resumes, and forks
when advertised. No MCP, file, or terminal capability is advertised. The
permission handler always answers `cancelled`; it never grants authority.

### Safe local execution contract

Run the commands below from the ego repository root on macOS or Linux. You need
an installed `ego`, Node with native TypeScript stripping (the audit used
Node 26.10.0), Python 3.11+ (the audit used 3.14.7), and the relevant library.
The Python SDK itself supports Python 3.10, but this example uses
`asyncio.timeout`, which requires 3.11+.

`isolated.sh` creates its own HOME, EGO_HOME, workspace, and config below
`${TMPDIR:-/tmp}/ego-acp-clients/` (or `ACP_EXAMPLE_ROOT` if set). It clears inherited environment variables,
including provider credentials, and sets `OLLAMA_HOST=127.0.0.1:1`. The selected
model is `ollama/qwen3`, with thinking off. ego runs with `--bare --no-tools`.
The example profile uses sandbox off to avoid host confinement prerequisites;
this is **not** a recommended production sandbox policy. Use it only for this
empty, tool-free probe. No paid model or external service is contacted.

The prompt is expected to **fail** when local provider discovery reaches the
closed loopback port. Success would be an error in this isolated probe. This
proves request/error handling, not successful model generation. Streaming
callbacks still receive real session updates and the error text replayed by
`session/load`. Cancellation is sent while idle; no cancellation of live model
work is claimed. The permission callback is provided but not invoked by the
tool-free probe. Resume is checked on the current connection, not after a crash.

The launcher prints the isolated directory and retains it for inspection.
Remove only those printed directories when finished. A production client must
supply its own approved model, authentication, timeout, sandbox, workspace trust,
and permission UI; do not weaken the isolation here to test a real account.

### TypeScript

Copy the example beside locally installed dependencies so Node can resolve the
package. Package downloads need network access; the client probe does not.

```sh
root="$PWD"
work="${TMPDIR:-/tmp}/ego-acp-ts"
mkdir -p "$work"
printf '{"type":"module"}\n' > "$work/package.json"
npm install --prefix "$work" --cache "$work/npm-cache" --ignore-scripts \
  --no-audit --no-fund @agentclientprotocol/sdk@1.9.0 zod@4.3.6
cp docs/integrations/examples/acp/client.ts "$work/client.ts"
sh "$root/docs/integrations/examples/acp/isolated.sh" node "$work/client.ts"
```

### Python

Install only in the temporary environment, not globally. Set the uv cache path
explicitly. Use an already installed Python interpreter; do not let uv download
one outside the temporary tree.

```sh
root="$PWD"
work="${TMPDIR:-/tmp}/ego-acp-python"
mkdir -p "$work"
UV_CACHE_DIR="$work/uv-cache" uv venv --python "$(command -v python3)" "$work/venv"
UV_CACHE_DIR="$work/uv-cache" uv pip install --python "$work/venv/bin/python" \
  agent-client-protocol==0.12.1
sh "$root/docs/integrations/examples/acp/isolated.sh" "$work/venv/bin/python" \
  "$root/docs/integrations/examples/acp/client.py"
```

### Rust

The standalone manifest pins ACP 3.4.0 and enables only `process` and the fork
feature. It has no dependency on an ego crate. Rust SDK 3.4.0 declares Rust
1.88.0 as its minimum version; dependency resolution can add requirements, so
use the current project toolchain and preserve the generated lockfile.

Build with the current toolchain, then run the client through the isolated launcher:

```sh
root="$PWD"
cargo build --manifest-path "$root/docs/integrations/examples/acp/rust/Cargo.toml"
sh "$root/docs/integrations/examples/acp/isolated.sh" /absolute/path/to/ego-acp-client-example
```

The executable must match the operating system on which you run the launcher.
These examples are protocol smoke checks, not full editor compatibility tests.
Permission handling and in-flight cancellation need separate integration tests.

### Observed local output

On 2026-10-16, installed `ego --version` reported `ego 0.1.0`.
The TypeScript and Python commands completed with exit 0. Both reported these
SDK-level milestones (session IDs and state paths are intentionally omitted):

```text
initialize 1
session/new ok
update available_commands_update
update session_info_update
_ego/pause paused
update config_option_update
session/prompt refused -32603
session/cancel sent (idle)
session/load ok
update user_message_chunk
update agent_message_chunk
session/resume ok
session/fork ok
```

The replayed message described the refused
`http://127.0.0.1:1/api/tags` connection. Ordering of asynchronous updates can
vary. ego also wrote sandbox-off and no-confinement diagnostics to stderr.
These are not JSON-RPC stdout. The TypeScript example closes the child's stdin
before transport teardown; without this EOF, the initial probe hung after the
last successful method and was stopped by the bounded runner. The corrected
probe exited normally. The Python probe initially exposed the missing text
block discriminator in the upstream-style construction; explicitly setting
`type="text"` corrected the example, with no ego runtime change.

## Adapt the examples without a new SDK

- Keep the update callback active while a prompt request is pending. A prompt
  response is its terminal result, not the stream. Render text chunks, tool
  updates, and configuration updates by their standard discriminator; retain
  metadata needed by your UI. Load replay is not a second live turn.
- To cancel an active turn, send `session/cancel` concurrently and keep awaiting
  the original prompt response and updates. Do not cancel only the local await
  or kill the process and call that an acknowledged cancellation. The bundled
  safe probe demonstrates only an idle cancellation notification.
- Replace the fail-closed permission handler with an explicit UI when needed.
  To select an offered option, TypeScript returns
  `{outcome: {outcome: "selected", optionId: option.optionId}}`; Python returns
  `RequestPermissionResponse(outcome={"outcome": "selected", "optionId": option.option_id})`;
  Rust uses `RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone()))`.
  Do not assume an option's position or label implies allow-once. Only use an
  ID from the current request, and answer cancelled if the user dismisses it.
- For persisted continuation, store the session ID and the admitted cwd, then
  reconnect and initialize before `session/load` or `session/resume`. The new
  process needs the same authorized ego session store. Fork returns a **new**
  session ID: use that ID for later prompts. The examples omit IDs from logs but
  retain the original ID in memory for their operations.
- To fork at a completed message, use `_meta.ego.atMessageId` only if the fork
  capability advertises `atMessage`. Do not copy CLI `--at-message` into a
  standard request root. See the extension contract for valid UUIDs and lineage.
- For an ego-specific method, use the extension seam in the table above, check
  its advertised version, and send its required `v: 1`. `_ego/pause`/`_ego/resume`
  are host holds, not aliases for standard prompt cancellation or
  `session/resume`. A hold can survive after the prompt finishes, so release it.

These recipes do not establish full TUICommander host compatibility. That proof
still uses the [production host boundary](acp.md#host-verification-boundary).
