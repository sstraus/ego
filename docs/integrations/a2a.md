# Agent2Agent

The current A2A listener publishes an agent card so another agent can discover
εgo and its advertised limits. It does not yet dispatch work to the run
controller: every valid A2A operational method returns MethodNotFound.
Use ACP or the inbound MCP run server for implemented execution surfaces.

`ego a2a --listen ENDPOINT` serves the implemented A2A endpoint. The endpoint is
a local socket path or a loopback address. Discovery is at
`/.well-known/agent-card.json`. A local endpoint relies on operating
system authentication; an address requires the admitted transport credential (`EGO_A2A_TOKEN`).
A public bind is refused when the implementation cannot demonstrate its required
security boundary, even if a parser flag names public binding.

The CLI accepts `-C`, `--session-dir`, and `--sandbox` for this served surface.
Endpoint admission is shared with other inbound protocols; A2A uses its own
HTTP/protocol contract after admission. No card advertisement enables an unimplemented method or permission to
change the session's tools, perimeter, or model capabilities.

Sources: [CLI setup](../../crates/ego-cli/src/a2a.rs),
[A2A crate](../../crates/ego-a2a/src/), and
[endpoint admission](../../crates/ego-core/src/endpoint.rs).
