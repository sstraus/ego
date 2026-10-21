# Instructions and memory

Project instructions and persistent memory serve different purposes. Instructions
enter model context through workspace trust and prompt admission. Memory is a
reviewable store; a stored entry does not automatically become a system instruction.

## Initialize project instructions

In a trusted interactive workspace, run `/init` to review a proposal for
`AGENTS.md` and a relative `CLAUDE.md` symlink to it. Confirm only after reading
the proposed changes. Existing instruction content is preserved. An independent
`CLAUDE.md`, or a link to another target, requires explicit reconciliation first.

Initialization still needs write authority. It does not grant trust or widen
confinement. After an accepted change, use `/refresh` when idle to request ordinary
instruction admission in a new prompt epoch. Do not assume an already-sent
provider request sees edited files.

## Inspect persistent memory

The `/memory` surface requires the native MDKB CLI, an initialized `.mdkb` index
in the selected scope, and an admitted trusted workspace. If the index is absent,
initialize it deliberately with `mdkb init` in that scope; ego does not create it
silently. Project scope is the default. User scope uses the ego user directory.

```text
/memory project list
/memory project show parser-convention
/memory user list
```

## Review a mutation

```text
/memory project set parser-convention "Parser convention" "Keep parsing separate from effects."
/memory project disable parser-convention
/memory project delete parser-convention
```

Quote titles and content that contain spaces. IDs are alphanumeric slugs with
hyphens or underscores; titles are limited to 50 characters. Mutations require
write permission and confirmation of the preview. Disable retains the record
with expired retrieval eligibility; delete removes the selected entry.

The operator surface does not consume model output or automatically inject memory
bodies into prompts. The model-side coordinator-only `memory_lookup` operation
is separate; see [tools](tools.md). Do not store credentials in either surface.

Source: [memory and instruction administration](../../crates/ego-cli/src/memory.rs).
