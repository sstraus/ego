# Project instructions

Read this page in the [hosted user guide](https://sstraus.github.io/ego/guides/memory.html).

Project instructions enter model context through workspace trust and prompt
admission.

## Initialize project instructions

In a trusted interactive workspace, run `/init` to review a proposal for
`AGENTS.md` and a relative `CLAUDE.md` symlink to it. Confirm only after reading
the proposed changes. Existing instruction content is preserved. An independent
`CLAUDE.md`, or a link to another target, requires explicit reconciliation first.

Initialization still needs write authority. It does not grant trust or widen
confinement. After an accepted change, use `/refresh` when idle to request ordinary
instruction admission in a new prompt epoch. Do not assume an already-sent
provider request sees edited files.

## External memory tools

Ego does not embed a memory store, vendor command, or model memory tool.
Run your memory CLI directly, or configure an external integration through
[generic hooks](../integrations/hooks.md). The external handler must implement
ego's hook input and result schema; Claude Code hook JSON is not imported.

Source: [instruction initialization](../../crates/ego-cli/src/instruction_init.rs).
