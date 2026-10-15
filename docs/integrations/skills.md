# Skills

A skill adds a named instruction capability inside a session. The model first
sees a bounded descriptor; permitted invocation admits the full body and rooted
resources. A skill is different from an executable agent file that starts a run.

## Discover and invoke

Sources are `/etc/ego/skills/NAME/SKILL.md`,
`$EGO_HOME/skills/NAME/SKILL.md` (default `~/.ego/skills`), and trusted
`<workspace>/.ego/skills/NAME/SKILL.md`. There is no `.claude/skills` alias.
The required `name` must match the containing directory and cannot collide with
fixed slash commands/aliases.

`/skills` lists admitted skills and failures. `/skills diagnose` diagnoses;
`/skills rescan` prepares a fresh catalogue at an idle boundary. `/NAME arguments`
or `/skills invoke NAME arguments` invokes a user-visible skill. The model invokes
the typed `Skill` operation; prose mentioning a slash command is not invocation.

## Frontmatter

A bounded strict YAML envelope uses standalone `---` delimiters followed by an
opaque Markdown body. Unknown/duplicate keys, unsafe YAML, and malformed values
refuse admission.

| Field | Purpose / default |
| --- | --- |
| `name`, `description` | Required identity and invocation description |
| `license`, `compatibility`, `metadata` | Optional portable metadata |
| `when_to_use`, `argument-hint` | Optional invocation guidance |
| `arguments` | Optional ordered names for argument substitution |
| `allowed-tools` | Optional space-separated scalar of canonical selectors; intersects authority |
| `disallowed-tools` | Optional subtractive selector scalar |
| `model`, `effort` | Optional scoped selection, under current capabilities/ceilings |
| `paths` | Optional admitted relative resource paths |
| `user-invocable` | True by default |
| `disable-model-invocation` | False by default |
| `context` | Absent uses parent; exact `fork` selects isolated child execution |
| `agent` | Existing config profile; requires `context: fork` |
| `background` | False by default; selects background child execution with fork |
| `bare` | False by default; scoped instruction posture |
| `hooks` | Optional invocation-scoped hook entries |

Both invocation paths disabled is an admission error. Allowed tools never grant
permission; disallowed tools subtract. `shell`, bang-command blocks, and
`${CLAUDE_*}` execution variables are refused rather than evaluated.

Arguments substitute once: `$ARGUMENTS` is the raw tail, `$ARGUMENTS[N]`/`$N` an
indexed value, and `$name` a declared position. Missing indexed values remain
literal; missing named values become empty. If no placeholder receives a value, `ARGUMENTS: <raw tail>`
is appended. Indexed values use shell-style quoting. A single backslash escapes
an argument placeholder; inserted values are never expanded again. Rendered text is not rescanned as shell code.

## Durability and authority

Instruction/resource bytes are captured durably before entering model context.
Resume/retry reuses admitted bytes rather than rereading a changed file. Rescan
publishes an immutable candidate atomically or retains last-good with diagnostics.
A skill's tools, hooks, selected model, and fork remain bounded by parent/system
permission, sandbox, offline, and budget authorities.

Sources: [loader](../../crates/ego-cli/src/skill.rs),
[domain skill service](../../crates/ego-core/src/skill.rs),
[skill tests](../../crates/ego-cli/src/skill/tests.rs), and
[retained format research](../internals/20-agent-skills.md).
