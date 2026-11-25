# Where to find what

Several ego features answer the same question from different places. This page
maps them. It adds no behaviour.

## See the effective state

| You want to see | Terminal | In a session |
| --- | --- | --- |
| Health of providers, credentials, sandbox | `ego doctor` | `/doctor` |
| Provider readiness and saved settings | `ego setup` (interactive) | `/setup` |
| Effective configuration with its sources | `ego config ls --effective` | `/config` |
| Why a call is allowed or refused | `ego rules explain TOOL JSON` | `/permissions` (effective rules) |
| Admitted hooks | `ego hooks ls` | `/hooks` |
| MCP servers and their admitted state | `ego mcp ls` (probes enabled servers) | `/mcp` |
| Usage, context and cost | `ego usage` | `/usage` |
| Saved accounts and subscription quota | `ego auth status`, `ego auth usage` | The `rate_limits` status-line segment |

## Change stored configuration

These commands all write the user configuration document. They differ in what
they change and who they are for.

| Command | Changes | For |
| --- | --- | --- |
| `ego config set\|unset\|edit` | Any managed key | People |
| `ego config capabilities\|snapshot\|batch` | Typed batches with an owner revision | Host programs that edit settings |
| `ego roots add\|rm` | Perimeter roots | People |
| `ego rules allow\|deny\|revoke\|remember` | Persistent permission rules | People |
| `ego mcp add\|rm` | One MCP server entry | People |
| `ego auth login\|logout\|remove` | Stored credentials and named connections | People |
| `--set KEY=VALUE` (`-c`) | One run only; nothing is stored | People and scripts |

Removal verbs differ by command: `roots rm`, `rules revoke ID`, `mcp rm`,
`config unset`, and `/config roots revoke PATH` in a session.

## Extra directories

| Name | Scope | Meaning |
| --- | --- | --- |
| `--allow-dir PATH[:ro\|rw]` (and `--read-only-dir DIR`, which is the read-only case) | One run | Grants one existing directory to this run |
| `ego roots add` and the `roots` key | Stored | Grants a directory to every run |
| `ego mcp-server --allow-root DIR` | One server launch | Lets a client choose `DIR` as the root of a run; it is not a grant to a run |

## Durable text in the model's context

| Mechanism | Who writes it | When it is read |
| --- | --- | --- |
| Instruction files (`AGENTS.md`) | You | At session start and at `/refresh`, only in a trusted workspace |
| Workspace memory (`memory` tool) | You and the model; opt-in | At session start and at `/refresh`, appended after the instruction files |
| `/pin TEXT` | You | Kept as a standing constraint outside the compaction summary |
| `/goal` and the `goal` tool | You, or the model when no goal is active | Persistent objective for the session |

See [project instructions and memory](memory.md).

## Background work

| Name | What it is |
| --- | --- |
| `/tasks`, `/detach` | You list, attach to or cancel background child sessions |
| `task` tool | The model starts an isolated depth-one child session |
| `exec` with `background`, `exec_status`, `exec_cancel`, `exec_monitor` | The model runs and watches a background process |

## Session state operations

Checkpoint, rewind, fork, compact and export are mapped in
[sessions and history](sessions.md).
