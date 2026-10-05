# Architecture

The architecture explains where policy lives, where real effects occur, and why
a host integration cannot bypass the same admission used by the CLI.

[ARCHITECTURE.md](../ARCHITECTURE.md) remains the canonical design and roadmap;
it stays at the repository root so existing project authorities remain stable.
[SPEC.md](../SPEC.md) records product scope and [CLI.md](../CLI.md) records the
command language. Implementation evidence comes from source and tests.

| Crate | Responsibility |
| --- | --- |
| `ego-core` | Pure configuration resolution, events, permissions, budgets, turns, durable run/session lifecycle, skills and hooks |
| `ego-tools` | Guarded filesystem/process/HTTP effects, private storage, evidence, credentials, and orchestration adapters |
| `ego-ai` | Provider rows, capability/pricing resolution, wire requests, decoding, and authentication flows |
| `ego-cli` | Composition, parsing, interactive terminal, output, diagnostics, and served protocol setup |
| `ego-acp` | ACP host projection over the durable core |
| `ego-mcp` | MCP protocol ownership and inbound/outbound adapters |
| `ego-a2a` | Agent2Agent delegation adapter |
| `ego-listen` | Shared endpoint admission and bounded listener transport |

Follow [concepts](concepts/README.md) for the work lifecycle,
[permissions](guides/permissions.md) for authority, and
[integrations](integrations/README.md) for host boundaries. Dated rationale is in
[internals/decisions](internals/decisions/); retained research and evidence are
indexed separately in [internals](internals/README.md).
