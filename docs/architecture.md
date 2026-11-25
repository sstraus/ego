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

## Terminal editor dependency

Ego pins a local copy of Reedline to preserve correct prompt placement when
external output interrupts editing, including when the prompt contains wide
or combining characters. The local changes belong to the editor; Ego keeps
its input policy and rendering decisions outside the dependency. Replace the
local copy when an upstream release passes the same terminal regression tests.
