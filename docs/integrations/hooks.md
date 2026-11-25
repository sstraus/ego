# Hooks

Hooks add lifecycle checks and source-tagged feedback. They are subtractive:
a hook can block or escalate work, but cannot grant a permit or relax a sandbox,
system deny, offline boundary, or budget. External integrations belong in configured
hooks, not built-in vendor bridges.

`ego hooks ls` inspects system/user hooks without loading project configuration.
`/hooks` inspects the active session's admitted catalogue, including trusted
project and invocation-scoped skill hooks. Neither command installs a hook.

## Declare a hook

Use ordered `[[hooks]]` entries in admitted εgo TOML configuration, or a skill's
`hooks` sequence. εgo does not import hook settings from other tools' files.
A TOML hook has `event`, optional `matcher`, `once`, `on_failure`, and a tagged
`handler`; every field/default is in [configuration](../reference/configuration.md).
A skill hook accepts only `event`, `matcher`, `once` and `handler`; `on_failure`
in a skill is refused, so a skill hook always uses the default (`allow`).

```toml
[[hooks]]
event = "PreToolUse"
matcher = "exec"
[hooks.handler]
type = "command"
command = "/absolute/path/to/your-check"
args = []
timeout = 30
```

Replace the program path with your real checker. This is a declaration shape,
not an included executable. Exact argv is used when `args` is present; otherwise
an explicitly supported interpreter spelling is required for shell behavior.
`timeout` is in seconds, from 1 to 3600.

Handlers are `command`, `http`, `mcp_tool`, or `prompt`. They use existing guarded
effects, egress, credentials, deadlines, cancellation, evidence, and budget owners.
Prompt handlers receive no tools. Unsupported handlers/fields fail by name; old
compatibility worksheets are not the current closed TOML schema.

## Lifecycle events

The current enum names:

- `SessionStart`, `InstructionsLoaded`, `UserPromptSubmit`, `UserPromptExpansion`
- `PreToolUse`, `PermissionRequest`, `PermissionDenied`, `PostToolUse`, `PostToolUseFailure`, `PostToolBatch`
- `PreModelSwitch`, `PostModelSwitch`, `SubagentStart`, `SubagentStop`
- `Stop`, `StopFailure`, `Interrupt`, `PreCompact`, `PostCompact`, `Elicitation`, `ElicitationResult`, `SessionEnd`

A matcher is read in one of two forms. A value that parses as a permission rule,
such as `read(src/**)`, `exec(git *)`, `net(*.example.com)` or
`tool(server/name)`, is kept as a rule and matches a tool event (`PreToolUse`,
`PermissionRequest`, `PermissionDenied`, `PostToolUse`, `PostToolUseFailure`)
when it covers one of the call's operations, as the permission layer reads a
rule: relative paths are anchored at the workspace, `exec` compares the argv as
the tool runs it (a shell string such as `sh -c "git push"` is one argv, not
decomposed), and `tool(server/name)` names an MCP tool. A call with several
operations matches when the rule covers any one of them. A rule never matches a
non-tool event. Any other value is an exact string comparison with the event's
selector, which for tool events is the tool name (for example `exec`).
Matchers are not regular expressions: a value written as
`/…/`, an empty value or one with control characters is refused. Without a
matcher, the hook runs for every occurrence of its event.
The configuration source preserves order: system, user, trusted project, then
active skills. A later source does not replace an earlier hook.

## Failure and replay

Admission validates a complete bounded candidate and retains last-good on reload
failure.

Hooks fail open. A hook that cannot run (launch failure, signal, non-zero exit
other than `2`, timeout, malformed response, denied handler effect, missing
backend) does not stop the turn or the session. ego records the failure in the
event stream and shows one notice per failed hook, for example:

```text
UserPromptSubmit hook h1:… failed to run (handler_failed): `/path/hook`: process terminated by signal SIGKILL
```

Only an explicit block decision from a hook that ran blocks: exit `2` or a
typed `block` decision.

The same rule applies to every event through one runner; no event has its own
failure path:

| Event | Hook cannot run | Explicit block |
| --- | --- | --- |
| `UserPromptSubmit`, `UserPromptExpansion` | prompt is sent; notice | prompt is refused |
| `PreToolUse`, `PermissionRequest` | tool call proceeds; notice | tool call is refused |
| `Stop` | turn completes; notice | generation continues with the feedback |
| `PostToolBatch`, `SubagentStop`, `PreCompact`, `PreModelSwitch`, `Elicitation`, `ElicitationResult` | effect proceeds; notice | effect is refused |
| Observers: `SessionStart`, `InstructionsLoaded`, `PermissionDenied`, `PostToolUse`, `PostToolUseFailure`, `PostModelSwitch`, `SubagentStart`, `StopFailure`, `Interrupt`, `PostCompact`, `SessionEnd` | run continues; notice | refused as a malformed result; run continues; notice |

`on_failure = "block"` opts one pre-effect hook in to blocking when its handler
cannot run; `on_failure = "allow"` is the default. `on_failure` is refused on
events that run after the effect. ego's own journal uncertainty is not a hook
failure: an unrecorded settlement or a start without settlement on replay still
blocks a pre-effect event. Post-effect failure cannot undo an effect that already
ran. Durable started/settled records prevent blind replay after ambiguous
delivery for lifecycle hooks.

`once=true` is meaningful only for invocation-scoped skill hooks. It is consumed
at durable invocation start, including failure, timeout, or uncertainty; it is
not a success-only retry trigger.

Sources: [hook domain/schema](../../crates/ego-core/src/hook.rs),
[event vocabulary](../../crates/ego-core/src/event/hook.rs),
[OS adapters](../../crates/ego-tools/src/hook.rs), and
[hook lifecycle tests](../../crates/ego-cli/tests/it/hook_lifecycle.rs).
The source schema is authoritative for accepted fields.

## Command input and returned context

Command stdin and HTTP bodies retain the versioned ego invocation envelope and
add the field names that hook payloads commonly use. The lifecycle owner supplies `session_id`,
`cwd`, and `hook_event_name`. `UserPromptSubmit` supplies `prompt`. Tool events
supply `tool_name` and the parsed JSON `tool_input`; successful completion adds
`tool_response` (the model-visible text), and failure adds `error`. `SessionStart`
supplies `source="startup"`. `Stop` supplies `transcript`: current-turn user and
assistant text, in journal order, capped at 8 KiB. This is inline text, not a
transcript-file path. Names and tool schemas remain ego's own.
Requests over 64 KiB fail visibly; prompt or tool input is not silently cut.

A handler can return context directly in a `hookSpecificOutput` envelope:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "additionalContext": "External context for this request."
  }
}
```

Supported context events are `SessionStart`, `UserPromptSubmit`, and
`PostToolUse`. The output event must match the invocation. Existing ego output
`{"version":1,"decision":"continue","feedback":"..."}` remains valid and can
include the same context envelope. Ego decision fields still require both
`version=1` and `decision`. An empty object is a no-op; a command that exits 0
without stdout is also a no-op. Unknown output fields are rejected, including
unsupported permission and output features of that envelope: ego accepts its context
shape only, not its policy decisions. Exit 2 still means veto.

Context is capped at 16 KiB per settlement, on a UTF-8 boundary, with a truncation
marker inside the cap. It is stored with the durable hook settlement. Generation
composition uses up to 32 KiB of attributed context, plus a fixed data disclaimer.
Session-start context is current for the session; prompt/tool context is current
only for its turn, including resumed generation in that turn. Changes and expiry
use append-only host-context snapshots or deltas after the complete history.
Earlier observations remain unchanged historical evidence, not current context.
Each block names the hook, lifecycle, and invocation. Blocks use user-role data,
not system authority, and do not grant permissions. Blocked outcomes do not
contribute context.

Ego accepts the existing `hookSpecificOutput` context shape rather than adding a
second context protocol.
