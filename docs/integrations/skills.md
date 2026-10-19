# Skills

A skill adds a named instruction capability inside a session. The model first
sees a bounded descriptor; permitted invocation admits the full body and rooted
resources. A skill is different from an executable agent file that starts a run.

## Discover and invoke

Native sources are `/etc/ego/skills/NAME/SKILL.md`,
`$EGO_HOME/skills/NAME/SKILL.md` (default `~/.ego/skills`), and trusted
`<workspace>/.ego/skills/NAME/SKILL.md`.

Trusted project discovery also recognizes `.agents/skills`, `.claude/skills`,
and `.codex/skills`, walking upward only to the nearest Git repository boundary.
Without workspace trust, project roots are not inspected. User roots also include
`~/.agents/skills`, `~/.claude/skills`, and the filesystem-backed skill directories
under `CODEX_HOME` and `CLAUDE_CONFIG_DIR`, with their normal home-directory
defaults. Platform-managed Claude and Codex skill directories are recognized too.

Nearest project roots take precedence over user roots; the native system root
is last. Discovery of a foreign directory does not import that client's policy.
Installed plugin caches, foreign plugin enablement, and dynamic synced skill
sources are not admitted. `/skills diagnose` and `/doctor details` explain these
limits. Install a reviewed standalone skill in an admitted root instead.

The required `name` must match the containing directory and cannot collide with
fixed slash commands/aliases.

`/skills` lists admitted skills and failures. `/skills diagnose` diagnoses;
`/skills rescan` prepares a fresh catalogue at an idle boundary. `/NAME arguments`
or `/skills invoke NAME arguments` invokes a user-visible skill. The model invokes
the typed `Skill` operation; prose mentioning a slash command is not invocation.

## Custom Markdown commands

A Markdown command is a user-invoked prompt template, not a shell script. Put
`review.md` in a trusted project's `.ego/commands/` to offer `/review`.
Nested files use colon namespaces: `git/review.md` becomes `/git:review`.
Project `.agents/commands`, `.claude/commands`, and `.opencode/commands` are also
recognized, with the same trust and repository boundary as skill discovery.
User sources include `$EGO_HOME/commands`, `~/.agents/commands`,
`~/.claude/commands`, `~/.opencode/commands`, and `~/.config/opencode/commands`;
the native system source is `/etc/ego/commands`.

```markdown
---
description: Review a named module
argument-hint: MODULE
---
Review $1 for correctness and report actionable defects. Do not edit files.
```

Invoke it with `/review parser`. Commands share the skill catalogue, help,
completion, diagnostics, and idle rescan boundary. They are user-invocable, not
model-invocable. Optional frontmatter accepts only `description` and
`argument-hint`; unsupported fields are refused, not silently ignored. The body
remains prompt text, including shell-looking text. It does not grant tools,
change models, or execute shell substitutions.

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

Sources: [root discovery](../../crates/ego-cli/src/capability_roots.rs),
[Markdown commands](../../crates/ego-cli/src/command.rs),
[loader](../../crates/ego-cli/src/skill.rs),
[domain skill service](../../crates/ego-core/src/skill.rs),
[skill tests](../../crates/ego-cli/src/skill/tests.rs), and
[retained format research](../internals/20-agent-skills.md).
