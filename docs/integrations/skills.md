# Skills

A skill adds a named instruction capability inside a session. The model first
sees a bounded descriptor; permitted invocation admits the full body and rooted
resources. A skill is different from an executable agent file that starts a run.

## Discover and invoke

Native sources are `/etc/ego/skills/NAME/SKILL.md`,
`$EGO_HOME/skills/NAME/SKILL.md` (default `~/.ego/skills`), and trusted
`<workspace>/.ego/skills/NAME/SKILL.md`.

Trusted project discovery also recognizes `.agents/skills`, `.claude/skills`,
`.codex/skills`, and `.github/skills`, walking upward only to the nearest Git repository boundary.
Without workspace trust, project roots are not inspected. User roots also include
`~/.agents/skills`, `~/.claude/skills`, and the filesystem-backed skill directories
under `CODEX_HOME` and `CLAUDE_CONFIG_DIR`, with their normal home-directory
defaults. Platform-managed skill directories are recognized too.

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

## Safety review

Before first use, Ego sends bounded skill instructions and declared hooks to
the active model in a separate request without tools or conversation history.
It also reviews text resources before delivery. If the review blocks the
content, the operator can allow that use once. Existing trust and permissions
still apply. Use `--no-skill-safety-check` for one launch or configure
`[skills] safety_review = false` in user settings. A project cannot disable
the review.

## Hide a skill without editing it

A skill checked into a repository or installed elsewhere cannot be hidden by
editing its frontmatter. List it in user or system configuration instead:

```toml
[skills]
hide = ["deploy"]   # the model cannot invoke it; you can still type /deploy
off = ["legacy"]    # neither the model nor you can invoke it
```

The lists are read from user, system and flag configuration only, so a project
cannot un-hide a skill the owner hid. A hidden skill leaves no diagnostic. Hiding
a skill that only the model could use removes it.

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
model-invocable. A command reads the same frontmatter fields as a skill:
`description`, `argument-hint`, `allowed-tools`, `disallowed-tools`, `model` and
`effort` behave as they do for a skill (the tool fields only narrow, a model is
only selected), and a field ego does not know is ignored. `context`, `agent`,
`hooks` and `arguments` have no effect on a command. A command body may contain
`` !`command` `` markers, run before the prompt is sent through the normal exec
permission path, and `@file` imports. A skill body has neither: to inject command
output, store the file as a command.

## Frontmatter

A bounded strict YAML envelope uses standalone `---` delimiters followed by an
opaque Markdown body. Unknown fields are inert. Duplicate keys, unsafe YAML,
and malformed supported values refuse admission. The spellings `allowed_tools`
and `disallowed_tools` refuse with the canonical hyphenated field name.

| Field | Purpose / default |
| --- | --- |
| `name`, `description` | Optional: directory name and first body paragraph by default; name may differ from directory |
| `license`, `compatibility`, `metadata` | Optional portable metadata; non-map metadata is ignored |
| `when_to_use`, `argument-hint` | Optional invocation guidance |
| `arguments` | Optional ordered names as a whitespace-separated string or list |
| `allowed-tools` | Optional string (spaces or commas) or list of canonical selectors; intersects authority |
| `disallowed-tools` | Optional string or list; maps reference tool names; ignores unknown names and pattern rules |
| `model`, `effort` | Optional scoped selection, under current capabilities/ceilings; `model: inherit` means no override |
| `paths` | Optional comma-separated string or list of globs saying when the skill applies. The model sees them as a hint appended to `when_to_use`; they never restrict anything |
| `resource-paths` | Optional list of admitted relative resource paths a model may read. ego-only |
| `user-invocable` | True by default |
| `disable-model-invocation` | False by default |
| `context` | Absent uses parent; exact `fork` selects isolated child execution |
| `agent` | Existing config profile; requires `context: fork`. `general-purpose` means no profile; `Explore` and `Plan` work when the configuration defines `[profile.Explore]` or `[profile.Plan]` |
| `background` | False by default; selects background child execution with fork |
| `bare` | False by default; scoped instruction posture |
| `hooks` | Optional invocation-scoped hook entries |
| `shell` | Inert metadata: exact `bash` or `powershell` |

Both invocation paths disabled is an admission error. Allowed tools never grant
permission; disallowed tools subtract. Plain booleans also accept `yes/no`,
`on/off`, and `1/0` (case-insensitive words). Quoted boolean strings still refuse.
Bang-command blocks are not executed.
`${CLAUDE_SKILL_DIR}` (the canonical skill directory), `${CLAUDE_SESSION_ID}`,
`${CLAUDE_PROJECT_DIR}` (the workspace root) and `${CLAUDE_EFFORT}` expand only
in source instruction text; caller argument bytes are never rescanned. A variable
with no value stays literal. `${CLAUDE_EFFORT}` is the reasoning effort of the
latest generation, so it stays literal in the first skill a user invokes before
any generation of the session.

Frontmatter that is not YAML at all (for example an unquoted
`description: Use when: x`) loads the skill with no fields. When the text names a
field that restricts the skill (`allowed-tools`, `disallowed-tools`,
`disable-model-invocation`, `user-invocable`, `context`, `agent`, `bare`, `hooks`,
`model`, `effort`, `resource-paths`) the skill is refused with a hint to quote the
value, because loading it without that field would widen it.

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
and [skill tests](../../crates/ego-cli/src/skill/tests.rs).
The [frontmatter table](#frontmatter) above lists every supported field.
