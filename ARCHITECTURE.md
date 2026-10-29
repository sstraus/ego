# Architecture

εgo separates model generation, policy, external effects, and user interfaces.
The terminal, headless CLI, and protocol servers use the same durable session
and run machinery.

## Crates

| Crate | Responsibility |
| --- | --- |
| `ego-core` | Events, configuration, permissions, budgets, turns, sessions, skills, hooks, and run control |
| `ego-ai` | Provider contracts, capabilities, pricing, wire formats, and authentication |
| `ego-tools` | Filesystem, process, and HTTP effects; storage, credentials, checkpoints, and orchestration adapters |
| `ego-cli` | Composition, command parsing, terminal interaction, output, and diagnostics |
| `ego-acp` | Agent Client Protocol projection over the durable core |
| `ego-mcp` | Inbound and outbound Model Context Protocol adapters |
| `ego-a2a` | Agent2Agent discovery |
| `ego-listen` | Shared endpoint admission and bounded listener transport |

## Events and state

Session state is reduced from an append-only event log. Events record prompts,
model output, tool calls and results, permissions, usage, questions, hooks,
subagents, and context transitions. Durable run records own run admission and
settlement. Protocol adapters project these facts; they do not create separate
policy authorities.

Session storage is private to the operator. Recovery, replay, and export preserve
session identity and ordering. Export applies redaction before producing public
artifacts. File checkpoints are separate Git recovery records, not replacements
for session history.

## Generation

A resolved generation profile binds the provider, model, wire format, capabilities,
credentials, and budget. A request does not infer unsupported capabilities from
a provider name. Unavailable or unknown capabilities remain explicit.

Provider adapters own request encoding and response decoding. HTTP and WebSocket
connections use guarded transports. Continuation state is connection-local where
required; durable conversation state belongs to the core.

## Tool authority

Tool input is parsed and prepared before policy approval. An approved operation
carries a permit bound to its target and policy. External-effect adapters consume
that authority to perform the operation. A model response alone cannot authorize
filesystem, process, or network access.

Permission mode and operating-system confinement are independent. Configuration
ceilings, credential floors, offline restrictions, and backend capabilities
remain applicable across user interfaces. An unavailable guarantee causes refusal,
not silent fallback to a weaker boundary.

## Interfaces

The interactive driver owns terminal input and rendering. Headless commands use
explicit stdout contracts, with diagnostics on stderr. ACP delegates user
interaction to a host while retaining session-scoped admission. MCP clients and
servers use their owning protocol adapters. A2A exposes discovery, not an alternate
run dispatcher.

## Further reading

- [Concepts](docs/concepts/README.md)
- [Permissions and confinement](docs/guides/permissions.md)
- [Command language](CLI.md)
- [Protocol integrations](docs/integrations/README.md)
- [Contribution guide](CONTRIBUTING.md)
