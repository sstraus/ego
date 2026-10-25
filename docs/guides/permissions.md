# Permissions and confinement

Permissions decide which prepared effects may run. Confinement limits what the
operating system allows an executor to do. Workspace trust decides whether project
configuration and instructions are admitted. These are separate authorities.

## Permission modes

| Mode | Purpose |
| --- | --- |
| `plan` | Prevent mutating operations; inspect and reason |
| `default` | Free reads, prompts for uncovered writes, honors allow rules and coordinator bounded admission |
| `auto` | Use deterministic low/bounded analysis, a judge where needed, then ordinary escalation |
| `accept-edits` | Admit workspace file edits; execution, network, opaque tools and external writes retain their normal checks |
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

Use `/mode` for the picker or `/mode <name>` for direct selection. Shift+Tab
cycles plan → default → auto → accept-edits → yolo → plan without submitting
or clearing the draft. Modals ignore it. During a turn, selections wait until
the turn ends and then stage the next generation. Unavailable modes report why
and leave the previous mode unchanged. The fixed sandbox cannot be relaxed.

Default remains a normal public mode with its existing policy. This selection
feature adds no new permission or session-approval mechanism. Existing rules,
ACP permission answers, denies and confinement checks keep their behavior.
`edits` remains an input alias for accept-edits and config emits `accept-edits`.
Snapshot v1 preserves its historical bytes and digests for saved-session resume.
Missing mode still uses default except for existing trusted-interactive auto.
Busy cycle intents resolve in the owner's queue order, so repeated keys and
preceding direct selections compose from the latest accepted staged mode.

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
ordinary escalation. Without an explicit judge selection, auto mode derives a
judge from the current provider/model, preferring a supplied review or small-model
default for supported providers. It does not mean that no judge runs.
`permissions.judge="none"` disables that role.
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
can lift the default for its matching read under `ro` or `workspace`. Project or flag allows do not. Under `off` and `adaptive`, built-in read, list, and search tools retain an immutable credential-path floor. This
is a read policy, not a subprocess secret filter or an assertion that all allowed
search output is redacted. Exec arguments are data, not file accesses: a command
may print or edit test text that names a credential path. The native sandbox
checks files when the child opens them, including paths built inside a script.
With `--sandbox off`, exec has no credential protection: off = no confinement,
credentials included. Permission checks on the command still apply, but read
tool rules do not constrain files opened by an unconfined child.

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

Built-in read, list, and search tools follow the admitted sandbox read reach.
Under `off` and `adaptive`, they can access non-credential paths outside the
workspace without an extra root. Each permit still covers only the named file
or traversal subtree; writes do not gain this reach. Adaptive also retains its
captured credential exclusions. Under `ro` and `workspace`, these tools retain
the workspace and admitted extra-root boundary.

Under strict levels, absent an explicit roots declaration, an existing `~/Gits` is added as a read-write
extra root. Adaptive omits that implicit root. Use `ego config set 'roots=[]'` to clear configured roots; local/flag
root declarations have stricter read-only constraints and cannot replace this
user declaration. The effective perimeter reports the resulting source.

For `ro` and `workspace`, outside files remain closed unless the native backend
admits their root. On macOS, credential-shaped files outside the workspace and
admitted extra roots also have a deny-read floor, even beneath runtime roots.
Linux's Landlock and namespace backends enforce admitted-root access; they do
not add a credential-pattern exclusion within runtime roots. An admitted data
root can contain credential-shaped files: native exec reach is separate from
the read tool's default-deny rules. Adaptive keeps its captured credential
exclusions, including their protection against symlink aliases.

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
and linked-worktree metadata, `TMPDIR`, an operator `CARGO_TARGET_DIR`, the pnpm
store (`PNPM_HOME`, `XDG_DATA_HOME/pnpm` or `~/Library/pnpm`, plus `/store`), the
pnpm cache (`XDG_CACHE_HOME` or `~/Library/Caches`, plus `/pnpm`) and the npm
`_cacache` and `_logs` (`npm_config_cache` or `~/.npm`) before commands.
Cargo/rustup defaults apply when their environment variables are absent. The pnpm
home, its `bin`, global virtual-store `store/v*/links`, cached `pnpm/dlx` tools
and npm `_npx` stay read-only: the host runs their code later. Admission protects
every captured store version's `links` path, or `store/v11/links` when no version
exists yet (pnpm 12's default). Store versions created later require fresh
admission to capture their host-code paths.
On macOS, admission also queries the installed pnpm for `store path`, `root -g`
and `bin -g` from the workspace. Each query runs under a read-only Seatbelt
profile with network disabled and a two-second limit. Project-selected pnpm
code never runs unconfined for discovery. Reported paths cannot grant writes
outside the operator-selected pnpm home. A relocated store outside that boundary
requires explicit authority rather than trusting project configuration.
For the recorded pnpm 11 layout, the reported bin and store version identify the
sibling `package-manager-store/vN`. Only its database companions, operation-lock
path and `tmp` are writable; engine links and the reported global directory
remain read-only. Missing or symlinked engine stores add a perimeter warning,
not a broad write grant. If pnpm requires writes to answer a query, admission
adds no query-derived roots and `/config perimeter` reports the declared pnpm
version and the discovery failure. It does not guess a layout from that version.
The existing default cache roots remain unchanged.

On macOS, strict runtime reads also admit the individual existing Git config and
ignore files (`~/.gitconfig`, and `git/config` plus `git/ignore` under
`XDG_CONFIG_HOME` or `~/.config`). A directory at one of those names is not a
runtime root; neighboring files do not become readable.

Those roots appear as `derived` in perimeter inspection, with no per-cache judge.
Configured ceilings and readonly roots remain binding. Shared-cache writes are
intentional. Workspace and derived toolchain/target executables remain runnable.

Adaptive's OS credential floors cannot be lifted by permission rules or human
sandbox grants: SSH/AWS/GnuPG, netrc, keychain/browser stores, provider/ego/Cargo
credentials and `.env*` outside the workspace. Git config/hooks and worktree
selectors stay protected against writes.
`AGENTS.md` and `CLAUDE.md` at any depth, and Markdown files beneath the admitted
home's `.codex`, `.claude` and `.claude-private` directories also stay read-only,
including beneath workspace, derived, human and judge write grants.
Metadata roots, module directories,
worktree entries, hooks and info directories cannot be relocated. Git can still
remove transient operation state and empty object/ref directories. Ordinary OS
privacy restrictions also remain. These OS floors are separate from the default
read rules described above.
Report the exact refused target and its task relevance. A failed mutation can
leave partial effects; do not assume replay is safe. In `auto`, a refused write
goes to the sandbox judge first (see
[Sandbox judge for residual writes](#sandbox-judge-for-residual-writes)); a human
grant question follows when the judge has no opinion or says no. A refused path
that the approved command line does not name (for example inside a script) also
raises that question. Paths inside the workspace, `/`, the home directory, a
workspace ancestor or a floor never raise one. Under Adaptive a question about a
path other than the program asks for write access, because reads are already open.

In the native interactive CLI, a separate sandbox prompt names the canonical
path and requested access. `[y]` grants that access for the current session and
runs the displayed command again. The retry can repeat earlier side effects.
The grant applies to later exec calls in the same session, not to a new,
forked, or compacted session. It does not edit saved roots or permission rules.
A command approval alone does not grant path access. Unattended sessions have
no terminal grant seat and remain fail-closed.

### Sandbox judge for residual writes

`sandbox="adaptive"` with permission mode `auto` and a configured judge lets the
judge open one refused write for the session. Other modes and
`permissions.judge = "none"` keep the human question.

1. A foreground `exec` child exits and its stderr names an EPERM or EACCES path
   outside the writable perimeter. Floors, the workspace and paths that are too
   broad stop here and the judge is not asked.
2. The judge sees the printed and canonical path, owner and kind (dotfile,
   system directory, other repository), at most 32 directory entries (names,
   sizes and types, never contents), the refusal line, the writable roots, the
   refused command, the task and the agent's reason. All of it is data, not
   instructions. Its answer must name that exact path.
3. ego checks a write verdict again: floors, system directories, breadth and the
   canonical identity of the path. Then it records a `sandbox_residual_decided`
   event and only after that opens the path for the rest of the session.
4. The command is not run again. Its result says that the path is now writable;
   the agent runs the command again if the result is still needed.

A judge grant is never persistent. A recommended `always` scope is recorded but
applies only to this session; only a human can store a permanent grant. A
judge-granted path stays non-executable: programs placed there cannot run.
One command can raise at most 3 judge decisions and one session at most 32.
A judge that does not answer in time (the command waits at most 10 seconds), an
invalid verdict and a refusal from a background command get no grant; the human
question follows.
`/config perimeter` lists judge-granted paths as `read-write` roots with source
`judge`. A sanitized export keeps the outcome and scope of each decision but
not the path or the reason.

Judge prompts are versioned with an evaluation set in
[`evals/sandbox-auto/v3`](../../evals/sandbox-auto/v3/README.md). A native test
gives every attack path of that set a forced write verdict and checks that the
executor refuses it. The cases that only the prompt protects (injected text and
other prompt-only cases) are listed in that test. Agent instruction files have
an executor write floor and do not depend on the judge prompt.

**Root-list traps:** `ego roots add PATH --access read` edits the stored list,
not the implicit default. The first addition replaces the default `~/Gits`
grant. Removing the final stored root with `ego roots rm PATH` leaves
`roots = []`; it does not restore the default. Use `ego config unset roots`
to remove the declaration and restore the implicit default, if no other layer
selects roots. `ego roots ls` shows stored roots as access/path columns, not the
effective perimeter. Its empty messages distinguish an absent declaration from
an explicit empty list. Root edits report saved, removed or unchanged roots and
remind the user that running sessions keep their current permissions.
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

Mode names, input aliases, and cycle order come from
[the shared mode definition](../../crates/ego-core/src/config/layer.rs).
[REPL actions](../../crates/ego-cli/src/repl/actions.rs) and
[key handling](../../crates/ego-cli/src/repl/seat/edit_mode.rs) route direct and
Shift+Tab selections through the same policy owner. This is the shipped story
586 surface, not the rejected proposal for an extra manual-approval mode.

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
they also name the current durable ego session ID. This ID belongs to ego,
not a host PTY or TUICommander session. The facts do not expose a full
effective-root list.
This guidance is captured in the session's prompt epoch; existing sessions can
retain the previous identity until an idle `/refresh` opens a new epoch.

The model learns about a judge grant from the refused command's result: the note
says that the path is writable for the rest of the session, that the command did
not run again and that programs placed there cannot run. It must not ask the
judge itself, edit roots or claim that a partially failed command is safe to
replay.

Sources: [model identity](../../crates/ego-core/src/turn/identity.rs),
[system-context assembly](../../crates/ego-core/src/turn/mod.rs), and
[generator-boundary coverage](../../crates/ego-core/tests/tool_catalogue.rs).

### User configuration write floor

Sandboxed child processes cannot write the operator user-configuration directory
(`EGO_HOME`, or `HOME/.ego` when unset). Explicit writable roots and runtime grants
do not lift this floor. The canonical `config.toml` target is protected too
when the file is a symlink outside that directory. Reads retain their existing reach. Sandbox `off` has no
such native confinement. Sandboxed admission refuses a config file with multiple
hardlinks; replace it with a copy or symlink so its authority has one captured
file identity.

On macOS, Seatbelt denies writes under the captured directory and prevents
unlinking its ancestor boundaries. Linux Landlock grants cannot subtract a
protected descendant from a writable root. Ego therefore refuses Workspace
admission if the workspace, scratch directory, or writable root overlaps the
user-configuration directory. The error names both paths; select a separate
workspace rather than widening authority. Operator config commands run outside
the model's process sandbox and retain their existing atomic transaction.
