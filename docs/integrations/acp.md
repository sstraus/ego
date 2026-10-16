# ACP and TUICommander

ACP lets a host own the conversation UI while εgo owns durable execution, policy,
and evidence. TUICommander is one host; an ACP connection itself is not proof of
full compatibility with every host capability.

For an installation-free custom-agent setup and editor context recipe, see
[IDE setup over native ACP](ide.md).

For wire shapes, fallback rules, source citations, and planned features, see
[ego extensions to ACP](acp-extensions.md).

## Launch and transport

For stdio, the host launches executable `ego` with argument `acp`. Protocol stdout
is reserved for ACP; diagnostics stay off that stream. `--listen ENDPOINT` selects
a local socket or an admitted network endpoint. Local sockets authenticate through
OS peer identity; network ACP uses the implemented official WebSocket adapter and
transport authentication (`EGO_ACP_TOKEN`) at `/acp`, not a custom token preface
on newline JSON-RPC.

Launch options include `-C`, `--session-dir`, `--profile`, `--instructions`,
`--system`, `--append-system`, `--bare`, `--tools`/`--no-tools`, and `--allow`/`--deny`. `--sandbox` is
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

## Session profile ceiling

A host can select a session profile in `session/new` and bound it by its machine
profile. Names resolve through the existing ego config layers; this does not
import `.tuic.json`. The host passes the names from that file and its machine
selection:

```json
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/workspace/repo","mcpServers":[],"_meta":{"ego":{"profile":"repo","ceilingProfile":"machine"}}}}
```

`profile` overrides the launch profile for this session. `ceilingProfile` is
optional and resolves from machine layers (system, user, environment), even if
the workspace is trusted. A repository cannot redefine the ceiling profile.
Omitting both fields preserves the existing launch behavior. Omitting only the
ceiling selects the requested profile without adding a host restriction.
Malformed names and unknown profiles refuse creation.

The effective mode and sandbox retain the narrower value. Extra roots are
compared after canonicalization using the system root-ceiling rule: roots
outside the machine perimeter are dropped; write access above a read-only
bound becomes read-only. Additional executable read roots are also bounded.
The workspace remains admitted. Administrator ceilings in `/etc/ego/config.toml`
still apply above the host clamp and retain their existing refusal semantics.

A successful result includes `_meta.ego` with the selected names, effective
values and warnings. For a repository profile with `mode="yolo"`, `sandbox="off"`
and an extra root outside a workspace-only machine profile:

```json
{"sessionId":"<uuid>","_meta":{"ego":{"profile":"repo","ceilingProfile":"machine","effective":{"mode":"default","sandbox":"workspace","roots":[],"execPaths":[]},"warnings":["mode yolo exceeds profile ceiling default; using default","sandbox off exceeds profile ceiling workspace; using workspace","root /outside exceeds profile ceiling; dropped"]}}}
```

Warnings are strings in request-field order (mode, sandbox, roots, executable
roots); an unchanged request has an empty array. Creation succeeds after a host
clamp. Later controls cannot exceed the admitted ceiling, and prompt preparation
carries both profile names in durable run selections. Deferred confinement
measurement still publishes the standard `config_option_update` after creation.

Creation writes the profile names into the durable session log header, before
the first prompt. The manifest projects these names from that header.
`session/load` and `session/resume` restore this creation authority and durable
run selections before measuring confinement or admitting MCP servers. Forks and
compacted successors inherit the same names through durable session lineage,
including successors with no run records yet. A missing or invalid ceiling
refuses reopening; it never becomes an absent ceiling. Reattaching to a live
profile seat also refuses changed profile policy instead of replacing its
pinned authority.

## Session initialization prompt

`ego acp --system "Use concise answers" --bare` sets launch defaults. As in
`ego run`, `--system` replaces workspace instructions, `--append-system` appends
to the selected prompt, and `--bare` disables ambient discovery. `--bare` with
`--instructions` is refused. Explicit prompt text still works in bare mode.
The agent identity and runtime facts retain the existing CLI behavior.

A host can select a prompt for one `session/new` request:

```json
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/workspace/repo","mcpServers":[],"_meta":{"ego":{"systemPrompt":{"mode":"append","text":"Use concise answers"},"bare":true}}}}
```

`systemPrompt` requires `mode` (`append` or `replace`) and string `text`.
Append extends the launch prompt, including its launch-wide append text.
Replace substitutes the session text for the launch prompt and its append text.
Both modes accept `@file`, resolved against the session workspace, as CLI flags
do. An empty string is valid. Invalid modes, types, missing fields, and unknown
fields inside `systemPrompt` refuse creation. `bare` must be a boolean.

Omitted fields preserve launch defaults. A session can enable bare mode but
cannot undo launch-wide `--bare` with `bare:false`. A session bare selection
also conflicts with launch-wide `--instructions`. These selections belong only
to the created session and remain in its stable system prefix across turns;
they do not become user messages. Run selections retain them for a later load
or fork after the session has started its run.
