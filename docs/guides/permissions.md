# Permissions and confinement

Permissions decide which prepared effects may run. Confinement limits what the
operating system allows an executor to do. Workspace trust decides whether project
configuration and instructions are admitted. These are separate authorities.

## Permission modes

| Mode | Purpose |
| --- | --- |
| `plan` | Prevent mutating operations; inspect and reason |
| `default` | Apply ordinary deterministic policy and escalation |
| `auto` | Use deterministic low/bounded analysis, a judge where needed (main model by default), then ordinary escalation |
| `edits` | Allow ordinary editing while retaining separate checks for higher-risk effects |
| `yolo` | Relax ordinary session approval; retain system denies, confinement, offline, budgets, and hard boundaries |

Trusted interactive sessions choose `auto` only when no mode is configured.
The user file `~/.ego/config.toml` can silently select `mode="yolo"` and
`sandbox="off"`. Omitting `--yolo` does not cancel these defaults. Mode and
sandbox are independent: an approval does not lift OS confinement, and `yolo`
does not turn the sandbox off. To request approvals and workspace confinement
explicitly, use `ego --mode auto --sandbox workspace`.

Headless work does not ask a human; unavailable required approval becomes exit 3.
`--plan` and `--yolo` are mode shortcuts. REPL `/mode`, `/plan`, and `/yolo`
use live session authorities; a system `max_mode` can refuse a selection.

A prepared call can contain several operations: `edit` needs read and write,
external tools need connection and opaque-call authority. Every required operation
must be covered; approving one does not approve the rest. Deny takes precedence.

## Rules and remembered decisions

Rules describe effects, not shell text pretending to be an exact invocation:
`read(src/**)`, `write(src/**)`, and `exec(git *)` are canonical predicate forms.
Use `ego rules ls --json` to inspect effective origins; `allow`/`deny` persist a
user predicate; `revoke ID` removes it from the selected user target only.
Copies in other authorities can survive revocation.

```sh
ego rules ls --json
ego rules explain read '{"path":"README.md"}'
```

`rules explain` prepares and matches without executing. `rules remember` previews
the narrowest expressible predicates and the omitted dimensions; it writes only
with `--yes`. Interactive remembered permissions use the same transaction. In the approval
prompt, `[d]` shows the parameters and proposed rules; `[a]` persists the displayed
allow rules. For a shell wrapper such as `bash -c 'git status'`, this is the exact
rendered argv string inside `exec(...)`, including the script. A different script
will not match. It is not a general grant for `bash` or for commands in the script.
Current code can suggest `exec(git status *)` for a bare recognized program and
subcommand; absolute program paths and interpreters retain exact argv. Cwd,
timeout, and environment are not represented in the persistent exec rule.
The live E2E used shell wrappers and absolute paths, so its remembered commands
stayed exact.

`Risk: unknown` means the analyzer cannot establish bounded effects. It does not
mean the command is known to be harmful. An exec can report `runs a program`,
`program not fully identified`, or `runs code from its arguments`, even for a
read-only shell script. `default` mode can ask for each such call. In `auto`,
deterministic analysis runs first, then a configured `permissions.judge`, then
ordinary escalation. No configured judge means there is no sentinel model.
The judge receives bounded task context and action evidence (see
[Judge action evidence](#judge-action-evidence)); it cannot classify a shell
script from content it is not shown. A configured
judge is not a promise that unknown exec calls will stop asking a human.

For example, select a judge in a user profile (it needs a usable credential and
supported model facts, just as the main model does):

```sh
ego config set --profile sentinel 'mode="auto"'
ego config set --profile sentinel 'permissions.judge="openai-codex/gpt-6.1-sol"'
ego --profile sentinel --sandbox workspace
```

Preview an exact exec rule without executing or saving it:

```sh
ego rules remember allow exec '{"argv":["/usr/bin/git","status","--short"]}'
```

Add `--yes` only after reviewing the predicates and omitted dimensions.
An ACP session grant is transient; a persistent grant is a user rule with the
same origin, preview, and limitations. “Allow all for this session” does not
persist a global allow or raise system ceilings.

Credential-like reads have built-in default denies, including `.env*`, `.ssh/**`,
`.aws/**`, `.netrc`, and keychain patterns. Only an effective explicit user allow
can lift the default for its matching read. Project or flag allows do not. This
is a read policy, not a subprocess secret filter or an assertion that all allowed
search output is redacted.

Default allows do not authorize control paths such as instruction/config/skill
material. Those require their dedicated authority. Explicit file-layer denies
and immutable system denies continue to win.

## Filesystem and network boundaries

| Sandbox | Boundary |
| --- | --- |
| `ro` | Read-only confinement; writes remain denied even under an extra writable root |
| `workspace` | Workspace writes with the admitted extra-root policy |
| `adaptive` | Broad reads and executable reach with hard credential exclusions; workspace, configured and captured toolchain writes |
| `off` | No filesystem confinement guarantee; classifiers/permission checks still apply |

Under strict levels, absent an explicit roots declaration, an existing `~/Gits` is added as a read-write
extra root. Adaptive omits that implicit root. Use `ego config set 'roots=[]'` to clear configured roots; local/flag
root declarations have stricter read-only constraints and cannot replace this
user declaration. The effective perimeter reports the resulting source.

Adaptive is the fresh-config default on macOS, independently of permission mode
`auto`. Other platforms keep the `workspace` default until they support adaptive
confinement. Effective config and doctor show the selected level and the platform
default reason.
Explicit `workspace`, `ro`, and `off` selections retain their behavior; no config
file is rewritten. Select those levels when their stricter or open reach is
required. Adaptive
currently requires macOS Seatbelt and successful native capability probes;
explicit adaptive selections on unsupported platforms fail admission with explicit alternatives. It captures
operator `CARGO_HOME`, `RUSTUP_HOME`, trusted mbx `cache_dir`, validated git common
and linked-worktree metadata, `TMPDIR`, and an operator `CARGO_TARGET_DIR` before
commands. Cargo/rustup defaults apply when their environment variables are absent.
Those roots appear as `derived` in perimeter inspection, with no per-cache judge.
Configured ceilings and readonly roots remain binding. Shared-cache writes are
intentional. Workspace and derived toolchain/target executables remain runnable.

Adaptive's OS credential floors cannot be lifted by permission rules or human
sandbox grants: SSH/AWS/GnuPG, netrc, keychain/browser stores, provider/ego/Cargo
credentials and `.env*` outside the workspace. Git config/hooks and worktree
selectors stay protected against writes. Metadata roots, module directories,
worktree entries, hooks and info directories cannot be relocated. Git can still
remove transient operation state and empty object/ref directories. Ordinary OS
privacy restrictions also remain. These OS floors are separate from the default
read rules described above.
Report the exact refused target and its task relevance. A failed mutation can
leave partial effects; do not assume replay is safe. Automatic residual-write
judgment is pending the shared judge integration; the existing human grant flow
still handles eligible refusals. A future judge-write grant will also forbid
execution within that residual root for the rest of its session.

**Root-list traps:** `ego roots add PATH --access read` edits the stored list,
not the implicit default. The first addition replaces the default `~/Gits`
grant. Removing the final stored root with `ego roots rm PATH` leaves
`roots = []`; it does not restore the default. Use `ego config unset roots`
to remove the declaration and restore the implicit default, if no other layer
selects roots. `ego roots ls` shows stored roots, not the effective perimeter.
Use `--profile NAME` consistently when editing a profile's roots.

`ego roots` accepts `read` and `read-write`, not exec. For extra executable
access, configure `exec_paths` through `ego config edit`; it is not a managed
`config set` key. A read or read-write root alone cannot authorize execution.
The workspace and backend runtime paths have separate built-in exec access.
System ceilings can refuse extra paths. See the macOS recovery entries in
[troubleshooting](troubleshooting.md).

Saved roots and `exec_paths` do not change a running session's captured perimeter.
Exit and resume without a positional prompt to admit the new configuration.
`/config perimeter` shows the current session; the commands below inspect a new
resolution, not that session.

System sandbox configuration is a floor. Extra roots specify canonical paths
and access; system roots cap them and configured executable paths. A read-only
root nested beneath a broader writable grant is refused rather than advertised
as subtraction that the backend cannot enforce.

```sh
ego config ls --effective --json
ego doctor --json
```

Inspection does not spawn confinement probes; capability evidence can remain
`not_checked`. An admitted session's `/config perimeter` uses its captured real
backend and roots, without reloading config.

`--offline`/`network="off"` forbids non-loopback egress from εgo-owned exits,
with guarded transport or demonstrated no-IP process confinement. It does not
control an independent local model daemon's upstream behavior. A requested
unsupported guarantee fails; no automatic fallback weakens it.

macOS uses Seatbelt. Linux uses admitted/proven Linux backends. Windows confinement
is open product work; transport/private-state support does not establish the
same filesystem/network boundary. See [troubleshooting](troubleshooting.md).

Sources: [policy engine](../../crates/ego-core/src/permit/grant.rs),
[rule grammar](../../crates/ego-core/src/permit/rule/),
[confinement adapters](../../crates/ego-tools/src/exec/), and
[effective-perimeter tests](../../crates/ego-cli/tests/effective_perimeter.rs).

## Judge action evidence

In `auto`, an unset `permissions.judge` prefers the same provider's review model
when it is in the catalogue: OpenAI Luna, Anthropic Haiku, or a dedicated Codex
reviewer. Otherwise it uses the active main model. It uses low reasoning effort
when supported, otherwise off.
Set `permissions.judge = "none"` to disable advisory generation and keep human
escalation. An explicit provider/model selects a separate judge. The judge sees
bounded task context, agent reasoning, recent call outcomes and action evidence.
Commands are data, not instructions. Same-provider judges receive raw argv;
cross-provider judges receive AST-backed redacted structure. Environment values
and repository file contents are not included. Raw-call hashes bind execution;
they do not replace the action description. See the
[action-evidence decision](../internals/decisions/2026-10-10-judge-action-evidence.md).

Current recovery behavior is implemented in [root mutations](../../crates/ego-tools/src/config.rs),
[remembered rules](../../crates/ego-core/src/config/rule_management.rs), and
[macOS profile rendering](../../crates/ego-tools/src/exec/profile.rs).

## Guidance available to the model

The built-in stable system identity contains a short permission recovery guide:
mode versus confinement, implicit roots, `exec_paths`, root-list traps, inspection
commands, and exit/resume after saved perimeter changes. The model must report a
block and ask the user for the required authority, not bypass it through another
route. Current runtime facts separately report the selected mode and sandbox;
they do not expose a full effective-root list or the ego session ID.
This guidance is captured in the session's prompt epoch; existing sessions can
retain the previous identity until an idle `/refresh` opens a new epoch.

Sources: [model identity](../../crates/ego-core/src/turn/identity.rs),
[system-context assembly](../../crates/ego-core/src/turn/mod.rs), and
[generator-boundary coverage](../../crates/ego-core/tests/tool_catalogue.rs).
