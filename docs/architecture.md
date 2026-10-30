# Architecture

The architecture explains where policy lives, where real effects occur, and why
a host integration cannot bypass the same admission used by the CLI.

[ARCHITECTURE.md](../ARCHITECTURE.md) describes the source structure and authority
boundaries. [CLI.md](../CLI.md) describes command conventions.

| Crate | Responsibility |
| --- | --- |
| `ego-core` | Pure configuration resolution, events, permissions, budgets, turns, durable run/session lifecycle, skills and hooks |
| `ego-tools` | Guarded filesystem/process/HTTP effects, private storage, evidence, and credentials |
| `ego-ai` | Provider rows, capability/pricing resolution, wire requests, decoding, and authentication flows |
| `ego-cli` | Composition, parsing, interactive terminal, output, diagnostics, and served protocol setup |
| `ego-acp` | ACP host projection over the durable core |
| `ego-mcp` | MCP protocol ownership and inbound/outbound adapters |
| `ego-a2a` | Agent2Agent discovery; no run dispatch |
| `ego-listen` | Shared endpoint admission and bounded listener transport |

Follow [concepts](concepts/README.md) for the work lifecycle,
[permissions](guides/permissions.md) for authority, and
[integrations](integrations/README.md) for host boundaries.
