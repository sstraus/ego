# Hooks

Hooks add lifecycle checks and source-tagged feedback. They are subtractive:
a hook can block or escalate work, but cannot grant a permit or relax a sandbox,
system deny, offline boundary, or budget.

`ego hooks ls` inspects system/user hooks without loading project configuration.
`/hooks` inspects the active session's admitted catalogue, including trusted
project and invocation-scoped skill hooks. Neither command installs a hook.

## Declare a hook

Use ordered `[[hooks]]` entries in admitted εgo TOML configuration, or a skill's
`hooks` sequence. εgo does not import `.claude/settings.json` or plugin hook maps.
The canonical shape has `event`, optional `matcher`, `once`, and a tagged
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
- `Stop`, `StopFailure`, `PreCompact`, `PostCompact`, `Elicitation`, `ElicitationResult`, `SessionEnd`

Event-native matchers are exact, not arbitrary JavaScript regular expressions.
The configuration source preserves order: system, user, trusted project, then
active skills. A later source does not replace an earlier hook.

## Failure and replay

Admission validates a complete bounded candidate and retains last-good on reload
failure. Pre-effect handler failure/malformed response/timeout blocks rather than
letting work proceed. Post-effect failure cannot undo an effect that already ran;
it becomes evidence. Durable started/settled records prevent blind replay after
ambiguous delivery.

`once=true` is meaningful only for invocation-scoped skill hooks. It is consumed
at durable invocation start, including failure, timeout, or uncertainty; it is
not a success-only retry trigger.

Sources: [hook domain/schema](../../crates/ego-core/src/hook.rs),
[event vocabulary](../../crates/ego-core/src/event/hook.rs),
[OS adapters](../../crates/ego-tools/src/hook.rs), and
[hook lifecycle tests](../../crates/ego-cli/tests/hook_lifecycle.rs).
The [compatibility research](../internals/21-claude-hook-parity.md) is retained
with its dated claims; use the current source schema for accepted fields.
