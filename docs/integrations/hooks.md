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
`hooks` sequence. εgo does not import `.claude/settings.json` or plugin hook maps.
The canonical shape has `event`, optional `matcher`, `once`, `on_failure`, and a tagged
`handler`; every field/default is in [configuration](../reference/configuration.md).

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

Event-native matchers are exact, not arbitrary JavaScript regular expressions.
The configuration source preserves order: system, user, trusted project, then
active skills. A later source does not replace an earlier hook.

## Failure and replay

Admission validates a complete bounded candidate and retains last-good on reload
failure. Pre-effect handler failure/malformed response/timeout blocks rather than
letting work proceed. `on_failure = "block"` (the default) states that rule;
`on_failure = "allow"` lets the guarded action continue after the handler failed
to start, timed out or crashed, and the failure stays in the event stream and in
the visible hook failure notice. An explicit `block` decision from the handler
always blocks, and an uncertain or unrecorded start still blocks. `on_failure` is
refused on events that run after the effect. Post-effect failure cannot undo an effect that already ran;
it becomes evidence. Durable started/settled records prevent blind replay after
ambiguous delivery for lifecycle hooks.

`once=true` is meaningful only for invocation-scoped skill hooks. It is consumed
at durable invocation start, including failure, timeout, or uncertainty; it is
not a success-only retry trigger.

Sources: [hook domain/schema](../../crates/ego-core/src/hook.rs),
[event vocabulary](../../crates/ego-core/src/event/hook.rs),
[OS adapters](../../crates/ego-tools/src/hook.rs), and
[hook lifecycle tests](../../crates/ego-cli/tests/it/hook_lifecycle.rs).

## Command input and returned context

Command stdin and HTTP bodies retain the versioned ego invocation envelope and
add Claude Code-compatible fields. The lifecycle owner supplies `session_id`,
`cwd`, and `hook_event_name`. `UserPromptSubmit` supplies `prompt`. Tool events
supply `tool_name` and the parsed JSON `tool_input`; successful completion adds
`tool_response` (the model-visible text), and failure adds `error`. `SessionStart`
supplies `source="startup"`. `Stop` supplies `transcript`: current-turn user and
assistant text, in journal order, capped at 8 KiB. This is inline text, not a
Claude Code transcript-file path. Names and tool schemas remain ego's own.
Requests over 64 KiB fail visibly; prompt or tool input is not silently cut.

A handler can return context directly in Claude Code's envelope:

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
unsupported Claude Code permission/output features; this is context compatibility,
not full Claude Code policy compatibility. Exit 2 still means veto.

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

Reference: [Claude Code hooks](https://code.claude.com/docs/en/hooks.md),
sections Common input fields, SessionStart, UserPromptSubmit, PostToolUse,
Stop, and JSON output. Ego accepts the existing `hookSpecificOutput` context
shape rather than adding a second context protocol.
