# Agent2Agent

The current A2A listener publishes an agent card so another agent can discover
εgo and its advertised limits. It does not yet dispatch work to the run
controller: every valid A2A operational method returns MethodNotFound.
Use ACP or the inbound MCP run server for implemented execution surfaces.

`ego a2a --listen ENDPOINT` serves the implemented A2A endpoint. A value that
parses as an IP address and port is a network address; anything else is a
local socket path. Discovery is at `/.well-known/agent-card.json`, and JSON-RPC
requests go to `POST /` with a body of at most 1 MiB; every other path answers
404. A local endpoint relies on operating system authentication; an address
requires a bearer token, read from `EGO_A2A_TOKEN`. The listener refuses to
start when that variable is unset or empty. Every request on an address,
including the agent card, is checked against the token first.

A loopback address is admitted as is. Any other address, including `0.0.0.0`
and `::`, is refused unless the operator also passes `--allow-public-bind`.
That flag makes the endpoint reachable from other machines. The listener speaks
plain HTTP without TLS, so the bearer token and every request cross the network
unencrypted; put it behind a TLS-terminating proxy or a private network before
using the flag.

The CLI accepts `-C` and `--sandbox` for this served surface.
Endpoint admission is shared with other inbound protocols; A2A uses its own
HTTP/protocol contract after admission. No card advertisement enables an unimplemented method or permission to
change the session's tools, perimeter, or model capabilities.

Sources: [CLI setup](../../crates/ego-cli/src/a2a.rs),
[A2A crate](../../crates/ego-a2a/src/), and
[endpoint admission](../../crates/ego-core/src/endpoint.rs).
