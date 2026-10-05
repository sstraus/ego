# MCP

MCP serves two different tasks: attaching external tools to a model, and exposing
εgo's own run controller to another MCP client. Both keep admission, authority,
visibility, and durable settlement explicit.

## Attach external servers

Use `ego mcp add` to store one complete stdio or HTTP server declaration;
`ego mcp rm` can remove a user entry, mask inheritance, or reveal an inherited
entry. `ego mcp ls` shows configuration and admitted state; `ego mcp reload`
reconciles current configuration. These operations do not silently log in.

Run with `--mcp NAME` to attach a named effective server or `--mcp @FILE` for a
strict invocation-scoped JSON server. A file entry is one complete server, not a
second configuration hierarchy. See [configuration](../reference/configuration.md)
for every field/default. HTTP OAuth uses `ego mcp login NAME` with the product's
CIMD binding; credentials are stored privately under the existing authority.

Project declarations load only after workspace trust. A stdio child has a
session-owned lifetime and an admitted working-directory/environment identity.
HTTP connection and tool calls use the guarded transport and captured generation
catalogue. Reload changes a future boundary, not an active call's authority.

The collapsed model surface starts with vendor-neutral bootstrap operations for
catalogue inspection and call selection. Discovery is a snapshot, not permission
to execute. A foreign call needs both connection authority and the exact opaque
operation/envelope authority. DNS evidence and continuations cannot widen a
captured policy or generation identity.

Protocol suspensions, elicitation, resources, and continuation handles remain
owned by the MCP adapter and durable core. A reconnection does not prove the
previous operation was never sent. Refused persisted sessions are quarantined;
inspect their recorded failure rather than repeatedly rebuilding untrusted state.

## Serve εgo runs

Configure a resolvable model first, for example `ego config set 'model="ollama/gemma4:e4b-mlx"'`.
If the model has no declared reasoning default, also save
`ego config set 'thinking="off"'` or another supported effort.
At least one effective token or money ceiling is mandatory; for example, save
`ego config set 'budget="20k"'`.
The server refuses startup without an effective configured model, effort, and budget. Its root
configuration must also support the requested confinement.

An MCP client launches `ego mcp-server` on stdio with required positive
`--max-concurrent-runs`, `--retained-requests`, and `--max-resource-bytes` values. It exposes the rooted run tools
`run`, `continue`, `cancel`, `status`, and `compact`, plus bounded run resources.
It is a controller over scoped εgo work, not a raw arbitrary shell server.

Requests carry stable identities; visibility and expected revisions guard control
operations. Compaction returns source/target identities and publication state.
Application failures use MCP `isError`; published uncertainty is a successful
non-retryable receipt, not an invitation to send another compact request.
There are no implicit defaults for those three launch limits.
For example, a client can pass `--max-concurrent-runs 1 --retained-requests 16
--max-resource-bytes 65536` as its explicit resource policy.

Sources: [CLI MCP management](../../crates/ego-cli/src/mcp.rs),
[inbound composition](../../crates/ego-cli/src/mcp_server.rs),
[protocol crate](../../crates/ego-mcp/src/),
[guarded registry](../../crates/ego-tools/src/mcp_registry.rs), and
[historical contract analysis](../internals/19-current-mcp-and-collapsed-tools.md).
