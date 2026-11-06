# MCP

MCP serves two different tasks: attaching external tools to a model, and exposing
εgo's own run controller to another MCP client. Both keep admission, authority,
visibility, and durable settlement explicit.

## Attach external servers

Use `ego mcp add` to store one complete stdio or HTTP server declaration;
`ego mcp rm` can remove a user entry, mask inheritance, or reveal an inherited
entry. `ego mcp ls` shows configuration and admitted state; `ego mcp reload`
reconciles current configuration. These operations do not silently log in.

Every enabled effective server attaches at session start; no flag is needed. Set `enabled = false` in a server entry to keep it
configured but detached. `--mcp NAME` selects an effective server explicitly,
including a disabled one; `--mcp @FILE` adds a strict invocation-scoped JSON
server. A server that admission refuses (for example any server under
`--offline`) or that fails to start is reported once with its cause and the
session continues; the same refusal for an explicit `--mcp` is a usage error. A file entry is one complete server, not a
second configuration hierarchy. See [configuration](../reference/configuration.md)
for every field/default. HTTP OAuth requires an explicit registration strategy;
see [remote OAuth login](#remote-oauth-login).

HTTP defaults to the current stateless contract. For a legacy SSE-only server,
select the profile explicitly:

```sh
ego mcp add events --http https://example.test/sse --profile legacy-sse
```

The stored entry uses `transport = "http"` and `profile = "legacy-sse"`.
The same fields work in an invocation-scoped JSON declaration. ACP hosts select
this profile with their explicit `sse` server variant. A failed current request
never triggers a legacy GET or initialize handshake. The legacy profile pins
2024-11-05, accepts only a same-origin POST endpoint, and keeps the configured
frame bound, deadline, cancellation, and guarded egress. It does not reconnect
or retry. Legacy complete results have no reusable cache grant. WebSocket is
not supported.

### Plaintext HTTP on the owner network

HTTP servers attach in every sandbox mode; only `--offline` refuses MCP. The
HTTP connection runs in the ego process, like the provider connection, and
reaches only the configured destination. HTTPS stays the default. A plaintext
`http://` server needs `allow_insecure_loopback = true` in its own entry, or
`ego mcp add NAME --http URL --allow-insecure-loopback`. Despite its name, this
per-server opt-in admits any host that you name: localhost, a LAN address, a
Tailscale address, or a host name. The owner decided on 2026-11-01 that a
server you configure explicitly is an intended destination, not a sandbox escape.

Retained protections:

- The opt-in is per server. It is invalid for an `https://` URL, so it never
  downgrades an HTTPS server.
- ACP hosts still get plaintext HTTP only for exact loopback URLs.
- Redirects are never followed, so a server cannot move a request or its
  credential to another URL.
- OAuth for an HTTPS server never moves to plaintext HTTP off this machine. For
  a plaintext server, a plaintext issuer must be on the same host as the server;
  an HTTPS issuer is always accepted.
- When a plaintext server off loopback uses `auth = "oauth"`, ego writes one
  warning per run (not with `--quiet`) and per `ego mcp login`: anyone on that network can read the
  credential. Move the server to HTTPS to remove the warning.

To withdraw the approval, remove `allow_insecure_loopback` from the entry or set
it to `false`. Ego then refuses that plaintext server again.

Project declarations load only after workspace trust. A stdio child has a
session-owned lifetime and an admitted working-directory/environment identity.
HTTP connection and tool calls use the guarded transport and captured generation
catalogue. Reload changes a future boundary, not an active call's authority.

The collapsed model surface starts with vendor-neutral bootstrap operations for
catalogue inspection and call selection. A `search_tools` response can include a
complete MCP definition in the sibling `top_schema` field, with its `name`
matching `results[0].name`.
Ego admits that top hit before the next generation, so the model can call
`call_tool` without a separate `get_tool_schema` round. The complete inline hit
must fit within 16 KiB of serialized JSON. Optional tool metadata is preserved.
Other hits and summary-only responses
still need an exact schema lookup. Servers own ranking and inline publication;
ego does not issue hidden lookup calls or pin server-specific tools.

Inline admission uses the captured search fingerprint and the same live-server
and current-generation checks as an exact lookup. Tool-list changes invalidate
stale search results. Both outer dispatch arguments and inner tool arguments
remain validated before effects. Discovery is a snapshot, not permission
to execute. A foreign call needs both connection authority and the exact opaque
operation/envelope authority. DNS evidence and continuations cannot widen a
captured policy or generation identity.

Protocol suspensions, elicitation, resources, and continuation handles remain
owned by the MCP adapter and durable core. A reconnection does not prove the
previous operation was never sent. Refused persisted sessions are quarantined;
inspect their recorded failure rather than repeatedly rebuilding untrusted state.

## Remote OAuth login

OAuth login, persistence, runtime attachment, and refresh are implemented.
In an interactive session, use `/mcp login NAME`. Ego opens the browser and
shows a copyable authorization URL without leaving the REPL. After successful
login, it reconnects that server so its tools are available in the same session.
The command does not change persistent server configuration.

The REPL selects DCR when it is the only eligible strategy advertised by the
issuer. When other strategies are available, an inline selector asks for the
strategy and, when needed, an operator-issued public client ID or a published
HTTPS CIMD URL. Escape or Ctrl-C cancels the selection. A failed strategy is
never retried with another strategy. During browser authorization, Ctrl-C
cancels the pending login; the callback and HTTP effects retain their deadline.

From a shell, choose exactly one explicit public-client registration strategy for an existing
HTTP server configured with `auth = "oauth"`:

```sh
ego mcp login NAME --client-id REGISTERED_PUBLIC_CLIENT_ID
ego mcp login NAME --cimd https://example.test/ego-client.json
ego mcp login NAME --dcr
```

These are alternatives, not three commands to run in order. A bare
`ego mcp login NAME` refuses and names the required selection. `--client-id`
uses an operator-issued public client ID; `--cimd` supplies a published HTTPS
client metadata document; `--dcr` explicitly requests issuer-advertised dynamic
registration. Confidential-client requirements are not silently bypassed.

The command prints a copyable authorization URL and waits at most 120 seconds
for the loopback callback. `--callback-port PORT` selects a registered port
(the default `0` selects a free port). For a remote shell, forward that callback
port to the machine running ego. The flow validates state, PKCE, issuer, and
exact protected-resource identity before storing the credential privately.

### Services with separate login and token hosts

By default the authorization, token and registration endpoints must share the
origin of the issuer that the protected resource names. A service that hosts
them elsewhere is refused with "OAuth metadata binding mismatch". To accept it,
enroll the extra origins for that one server with repeatable `--oauth-host`
options on `ego mcp add NAME --http URL --auth oauth`, or set
`oauth_hosts = ["https://login.example.com", ...]` on the server entry.
Each value is a bare `https` origin (loopback `http` is also allowed): no path,
query, fragment or userinfo.

**Approval scope.** Only configuration enrolls hosts, and only for the server
it appears on. Discovery metadata never adds a host. A project configuration
that lists `oauth_hosts` shows them in the trust prompt like any other
executable surface. The enrolled origins may host the authorization, token and
registration endpoints; the authorization code, PKCE verifier and registration
request go to a host only when it is the issuer origin or is enrolled.

**Retained protections.** The issuer must still match the protected-resource
document; the metadata `issuer`, `resource` and `audience` must still bind to
the exact resource; PKCE (`S256`) and `state` are still validated; the
callback `iss` is still checked; and a public client is still required.
Credentials are stored under the issuer and resource, never under an enrolled
host, and are sent only to the MCP server URL.

**Recovery.** Remove the origin (edit `oauth_hosts`, or run `ego mcp add`
again without `--oauth-host`) and run `ego mcp login NAME` again to return to
issuer-origin-only behavior. Changing the enrolled set changes the server
fingerprint; apply it to a running session with `ego mcp reload NAME`.

**Verification boundary:** no affected live service was tested. The behavior
is covered by fixtures with the metadata shape; an owner-selected real service
must still be enrolled and its metadata recorded before this is called
live-verified.

Runtime admission uses the stored credential and exact-resource locator.
Expiry and a 401 can refresh credentials; a selected operation is not replayed
because it returned 401. Missing or revoked credentials report a login hint.
Explicit non-interactive refresh uses the same registration selection, plus
`--refresh --issuer URL` and optional `--account ACCOUNT`.

OAuth behavior is covered by recorded public discovery documents and local
authorization-server tests. These fixtures do not guarantee that a third-party
service accepts a particular account or registration. Verify login and attachment
with the actual service. Do not inspect or publish native credential-store contents.

Sources: [CLI login and refresh](../../crates/ego-cli/src/mcp.rs),
[OAuth protocol and runtime](../../crates/ego-mcp/src/oauth.rs),
[loopback and persistence adapters](../../crates/ego-tools/src/mcp_oauth.rs),
[recorded public contracts](../../crates/ego-mcp/tests/fixtures/oauth/), and
[local authorization-server coverage](../../crates/ego-mcp/tests/it/oauth_local_server.rs).
Use `ego mcp login --help` for the installed binary's exact options.

## Serve εgo runs

Configure a resolvable model first, for example `ego config set 'model="ollama/gemma4:e4b-mlx"'`.
If the model has no declared reasoning default, also save
`ego config set 'thinking="off"'` or another supported effort.
At least one effective token or money ceiling is mandatory; for example, save
`ego config set 'budget="20k"'`.
The server refuses startup without an effective configured model, effort, and budget. Its root
configuration must also support the requested confinement.

An MCP client launches `ego mcp-server` on stdio. The launch limits are finite and optional:
`--max-concurrent-runs` (default 1), `--retained-requests` (default 16), and `--max-resource-bytes`
(default 65536); each must be positive. It exposes the rooted run tools
`run`, `continue`, `cancel`, `status`, and `compact`, plus bounded run resources.
It is a controller over scoped εgo work, not a raw arbitrary shell server.

Requests carry stable identities; visibility and expected revisions guard control
operations. Compaction returns source/target identities and publication state.
Application failures use MCP `isError`; published uncertainty is a successful
non-retryable receipt, not an invitation to send another compact request.
Owner selection policy: by default a client may select only the deployment root (`-C`, or the
current directory), the configured model, and the configured mode. The owner adds reviewed
alternatives at launch with the repeatable `--allow-root DIR`, `--allow-model P/M`, and
`--allow-mode MODE`. A request outside that set is refused before any run starts; the thinking
level, budget ceiling, and profile limits are unchanged, and every run still passes normal run
admission and the concurrency bound. An extra root is resolved at startup and must exist; it runs
with the same user configuration and ceilings as the deployment root, and project configuration
stays untrusted. `ego://capabilities` reports the active policy and limits. To recover from a
too-wide policy, restart the server without the `--allow-*` flags.

Sources: [CLI MCP management](../../crates/ego-cli/src/mcp.rs),
[inbound composition](../../crates/ego-cli/src/mcp_server.rs),
[protocol crate](../../crates/ego-mcp/src/),
[guarded registry](../../crates/ego-tools/src/mcp_registry.rs).

## Turn time

The live runtime context includes the current local date-time and UTC offset on
each generation. The system-clock adapter reads the host timezone. This line is
volatile context, not part of the stable system-prompt prefix. The model does not
need to execute a shell command to learn the current time.
