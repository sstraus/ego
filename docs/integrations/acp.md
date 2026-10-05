# ACP and TUICommander

ACP lets a host own the conversation UI while εgo owns durable execution, policy,
and evidence. TUICommander is one host; an ACP connection itself is not proof of
full compatibility with every host capability.

## Launch and transport

For stdio, the host launches executable `ego` with argument `acp`. Protocol stdout
is reserved for ACP; diagnostics stay off that stream. `--listen ENDPOINT` selects
a local socket or an admitted network endpoint. Local sockets authenticate through
OS peer identity; network ACP uses the implemented official WebSocket adapter and
transport authentication (`EGO_ACP_TOKEN`) at `/acp`, not a custom token preface
on newline JSON-RPC.

Launch options include `-C`, `--session-dir`, `--profile`, `--instructions`,
`--append-system`, `--tools`/`--no-tools`, and `--allow`/`--deny`. `--sandbox` is
the weakest confinement a session may choose, not an unconditional host grant.
`--allow-public-bind` alone does not establish network authentication.

ACP listener defaults are 32 simultaneous connections, 24-hour connection
lifetime, and 128 MiB cumulative ingress per connection. `--max-connections`,
`--connection-lifetime`, and `--max-ingress-bytes` tighten these explicit bounds.
Source: [listener limits](../../crates/ego-listen/src/limits.rs).

## Sessions, permissions, and controls

ACP sessions map to the durable core. Host sessions can load/list/resume/fork and
use supported lifecycle controls advertised by this build. A host's trust answer
belongs to its session. Permission UI maps prepared-call approval onto current
session/user rule authority; unsupported or malformed replies refuse with evidence.
Session-wide allowance is transient and still cannot raise system ceilings.

Model, effort, mode, confinement, queue/steering, pause/resume, and compaction are
capability-negotiated controls. εgo extensions are versioned under `_ego`; read
what initialization advertises rather than guessing from a product version.
`ego doctor --json` reports the local ACP surface without opening a hosted run.

The `_ego/compact` extension identifies the successor and makes it available to
the next standard session prompt. Standard tip forks and εgo's CLI completed-message
fork selector have different surfaces; do not assume all CLI flags are ACP fields.
Subscription usage is reported without pretending that an unknown monetary rate
is USD 0.

## Host verification boundary

The accepted TUICommander proof boundary is the packaged production backend API
and its named instance, not a hand-written client fixture or a screenshot. Source
and deterministic adapter tests document behavior, but this documentation task
does not complete the outstanding final host/product proof.

Sources: [ACP composition](../../crates/ego-cli/src/acp.rs),
[ACP adapter crate](../../crates/ego-acp/src/),
[permission adapter](../../crates/ego-cli/src/acp_permission.rs),
[host-boundary decision](../internals/decisions/2026-09-11-tuicommander-backend-api-is-the-host-boundary.md),
and [host proof tests](../../crates/ego-cli/tests/regression_case.rs).
