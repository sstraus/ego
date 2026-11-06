# Permissions and confinement

Read this page in the [hosted user guide](https://sstraus.github.io/ego/guides/permissions.html).

Permissions decide which prepared effects may run. Confinement limits what the
operating system allows an executor to do. Workspace trust decides whether project
configuration and instructions are admitted. These are separate authorities.

## Permission modes

| Mode | Purpose |
| --- | --- |
| `plan` | Prevent mutating operations; inspect and reason |
| `default` | Free reads, prompts for uncovered writes, honors explicit allow rules |
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

`edits` is an input alias for `accept-edits`; configuration emits the canonical
name. Repeated mode changes during a turn are applied in queue order. Rules,
ACP permission answers, denies, and confinement still apply after a mode change.

A prepared call can contain several operations: `edit` needs read and write
for each file, and external tools need connection and opaque-call authority.
Every required operation must be covered before execution; approving one does
not approve the rest. Deny takes precedence.

## Rules and remembered decisions

Rules describe effects, not shell text pretending to be an exact invocation:
`read(src/**)`, `write(src/**)`, and `exec(git *)` are canonical predicate forms.
Use `ego rules ls --json` to inspect effective origins; `allow`/`deny` persist a
user predicate; `revoke ID` removes it from the selected user target only.
Copies in other authorities can survive revocation.

```sh
ego rules ls --json
ego rules explain read '{"files":[{"path":"README.md"}]}'
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
Shell wrappers and absolute paths therefore keep their exact invocation rules.

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

Credential-like native reads have built-in default denies, including `.env*`,
`.ssh/**`, `.aws/**`, `.netrc`, and keychain patterns. An exact user-layer rule,
for example `read(/absolute/project/.env)`, lifts the default for that canonical
file. Under Adaptive it also lifts the native read perimeter for that file.
Wildcard, project and flag rules do not grant this exception. A read grant does
not approve a sibling file or credential children beneath an approved directory;
recursive list/search operations still check each canonical entry. Exact user
`list(...)` and `search(...)` rules likewise approve only their named protected
root. Explicit denies and the admitted workspace/root boundary still win.

Use `ego rules allow 'read(/absolute/project/.env)'` to save the owner's narrow
approval. Inspect its ID with `ego rules ls --json` and remove it with
`ego rules revoke ID` to revoke future rule authority.

The same exact user-layer rule is the value-disclosure approval. For the named
canonical file, `read` returns the file bytes unchanged, including formats that
have no key-name projection (SSH keys, `.netrc`, JSON tokens), and `grep` over
that file matches and shows its real lines when the rule is `search(...)`.
Without an exact user rule for the operation, native reads and searches of
credential files return key names only and unsupported formats refuse. Project
rules, flags, wildcards, permission modes (including `yolo`), sandbox `off`,
session approvals and approval of a parent directory never disclose values. A
disclosed value enters the model context and the session transcript; the usual
masking of known secrets still applies to command output. To stop disclosure,
revoke the rule; the next read returns key names or is refused again.

Every way to store such an exact user `read(...)` or `search(...)` rule for a
credential file grants disclosure:

- `ego rules allow 'read(/absolute/project/.env)'`;
- `ego rules remember allow read '{"files":[{"path":"/absolute/project/.env"}]}' --yes`;
- `[a]` (remember) in the interactive approval prompt, which is offered for a
  credential read when sandbox `off` removes the credential floor;
- `Allow always` in an ACP host prompt for the same read;
- a `read(...)` or `search(...)` entry that you write in your user
  configuration by hand.

Each remember surface (the `[d]` details and the `rules remember` preview, the
`[a]` prompt and the ACP prompt) states that remembering will show the values
of the named credential file.

Sandbox `off` removes built-in credential admission floors for native read, list
and search, as well as subprocess confinement. Operation scope, explicit denies,
permission-mode decisions and the key-name projection (without an exact user rule) still apply. Under
Adaptive, ungranted credential reads remain blocked even with `yolo` or a broad
allow. Exec arguments are data, not file accesses: the native process sandbox
checks files when the child opens them, including paths built inside a script.
Native-tool user rules do not widen a child process's OS credential exclusions.

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
or traversal subtree; writes do not gain this reach. Adaptive retains its captured credential exclusions except for exact user
native-tool grants described above. Under `ro` and `workspace`, these tools retain
the workspace and admitted extra-root boundary.

Under strict levels, no extra home-tree root is granted by default. Use the user
`roots` list to grant additional paths. `roots=[]` clears configured roots;
adaptive derived roots remain. The effective perimeter reports the source.

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
operator `CARGO_HOME`, `RUSTUP_HOME`, validated git common
and linked-worktree metadata, `TMPDIR`, an operator `CARGO_TARGET_DIR`, the pnpm
home (`PNPM_HOME`, `XDG_DATA_HOME/pnpm` or `~/Library/pnpm`), the
pnpm cache (`XDG_CACHE_HOME` or `~/Library/Caches`, plus `/pnpm`) and the npm
`_cacache` and `_logs` (`npm_config_cache` or `~/.npm`) before commands.
Cargo/rustup defaults apply when their environment variables are absent.
External build-cache locations require ordinary configured roots. Ego does not
read another tool's configuration to derive write authority.

On macOS, Workspace and Adaptive grant read/write access to the **whole operator
pnpm home**, including its binaries, engine store, links and global environments.
This is an intentional shared-toolchain write authority, not a content-only
cache. Cached `pnpm/dlx` tools outside that home and npm `_npx` stay read-only.
No pnpm subprocess runs during admission. These grants apply to all children,
including scripts and test runners that spawn pnpm.

Both modes also grant the exact current-user operation-lock directory,
`/private/tmp/pnpm-store-operation-locks-<UID>`, and the two system-info selectors
`vfs.disk-space` and `net.link.addr`. An existing operation-lock alias,
non-directory, foreign owner or group/other permission bits refuse admission.
No shared temp root or writable pnpm ancestor is added. Ancestor `_tmp_*` probes
remain denied. Read-only mode receives none of these additional grants.

Custom stores inside the workspace or pnpm home are already covered. After a
pnpm permission refusal, project/user `.npmrc` `store-dir` and top-level
`pnpm-workspace.yaml` `storeDir` are read as bounded data to identify an external
store for the existing interactive path approval. Configuration alone grants
nothing. Project YAML takes precedence over project `.npmrc`, then user `.npmrc`.
Environment interpolation is not executed by this hint reader; unresolved values
fall back to ordinary path-denial handling. Credential and configured read-only
floors still apply to all grants.

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
not the running session perimeter. Removing the final stored root with `ego roots rm PATH` leaves
`roots = []`; it does not grant extra paths. Use `ego config unset roots`
to remove the declaration and inherit other layers, if any. `ego roots ls` shows stored roots as access/path columns, not the
effective perimeter. Its empty messages distinguish an absent declaration from
an explicit empty list. Root edits report saved, removed or unchanged roots and
remind the user that running sessions keep their current permissions.
Use `--profile NAME` consistently when editing a profile's roots.

`ego roots` accepts `read` and `read-write`, not exec. For extra executable
access, use `ego config set 'exec_paths=["/absolute/path/to/bin"]'` or
edit `exec_paths` through `ego config edit`. A read or read-write root alone cannot authorize execution.
The workspace and backend runtime paths have separate built-in exec access.
System ceilings can refuse extra paths. See the macOS recovery entries in
[troubleshooting](troubleshooting.md).

Saved roots and `exec_paths` do not change a running session's captured perimeter.
Exit and resume without a positional prompt to admit the new configuration.
`/config perimeter` shows the current session; the commands below inspect a new
resolution, not that session.

**Terminal root approval:** when a built-in read, list, search or write is
refused outside the workspace, the human terminal that owns the session asks
for that exact canonical path and access: `Deny`, `Session` or `Permanent`.
The question shows the perimeter after approval: the workspace, every root that
stays (`kept`) and the new root (`new`). An approval adds a data root only; it
adds no exec grant. Exec reach stays separate (`exec_paths` or an owner sandbox
grant).

Neither choice changes the turn that asked. When that turn ends, ego re-admits
the same session in the same process through normal admission: it captures the
new execution observation, rebuilds the policy engine and the OS confinement for
new child processes, and keeps every earlier root. A configuration edit or a
restart is not necessary. On Linux, a new read root moves confinement from the
namespace backend to Landlock, so the terminal asks once to re-admit the
changed execution policy. If a background command or a background agent is
still running, re-admission waits until that work ends; the terminal reports
this. A running OS guard is never changed in place.

`Session` keeps the root for this session only; `ego resume` of the same session
restores it, and other sessions do not get it. If a session root is removed from disk, ego drops it
with a note on the next re-admission or resume; the other roots stay. `Permanent` also saves the root
with its journal receipt in the selected user-profile configuration.

The approval cannot widen itself. A root is the exact canonical path, not its
parent directory. A write inside a read root is refused as a read-only root. A
system root ceiling refuses a path or access outside it before the terminal
asks. Model or tool text cannot answer the question.

`/config roots` lists the session targets: session and permanent roots with
their access, owner sandbox grants (`read`, `write` or `exec`), and the revoke
command. `/config roots revoke EXACT_PATH` removes one of them.

Exec through a data root differs per backend: macOS gives a data root no exec
access. On Linux, a read root gives no exec access, but a read-write root is
also executable for child processes under both Landlock and the namespace
backend.

System sandbox configuration is a floor. Extra roots specify canonical paths
and access; system roots cap them and configured executable paths. A read-only
root nested beneath a broader writable grant restricts built-in tools. macOS
also enforces it for child processes. Linux Landlock reports that the writable
ancestor wins for child processes; it cannot subtract the nested grant.

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
is not supported; transport/private-state support does not establish the
same filesystem/network boundary. See [troubleshooting](troubleshooting.md).

Mode names, input aliases, and cycle order come from
[the shared mode definition](../../crates/ego-core/src/config/layer.rs).
[REPL actions](../../crates/ego-cli/src/repl/actions.rs) and
[key handling](../../crates/ego-cli/src/repl/seat/edit_mode.rs) route direct and
Shift+Tab selections through the same policy owner. All input routes select the same supported modes.

Sources: [policy engine](../../crates/ego-core/src/permit/grant.rs),
[rule grammar](../../crates/ego-core/src/permit/rule/),
[confinement adapters](../../crates/ego-tools/src/exec/), and
[effective-perimeter tests](../../crates/ego-cli/tests/it/effective_perimeter.rs).

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
they do not replace the action description.

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
[generator-boundary coverage](../../crates/ego-core/tests/it/tool_catalogue.rs).

### User configuration write floor

Sandboxed child processes cannot write the operator user-configuration directory
(`EGO_HOME`, or `HOME/.ego` when unset). Explicit writable roots and runtime grants
do not lift this floor. The canonical `config.toml` target is protected too
when the file is a symlink outside that directory. Reads retain their existing reach. Sandbox `off` has no
such native confinement. A config file with multiple hardlinks does not refuse
admission: ego finds every other name of the file inside the writable roots and
protects each one like the file itself. A name that lies outside every writable
root cannot be written by the sandbox. A scan that would visit more than
1,000,000 entries refuses admission, because ego cannot then prove that no alias
is writable.

Unrelated work is admitted when a writable root overlaps the configuration; only
writes to the configuration are denied. On macOS, Seatbelt denies writes under the
captured directory and prevents unlinking its ancestor boundaries. Linux Landlock
grants are additive and cannot subtract a protected descendant, so ego grants the
overlapping root entry by entry around the protected path. Writes below every
sibling are allowed. Inside a directory that holds the configuration, existing
files stay writable, but ego cannot create, delete or rename entries directly in
that directory or in the directories between it and the root; symlinks there are not
granted. A root inside the configuration directory gets no write access. Where the
bwrap backend would be selected, it refuses the overlap and ego uses Landlock. No
command, rule or grant lifts this floor: the model cannot give itself authority
over the configuration. Recovery: change the configuration with `ego config` or
your editor outside the sandbox, or select a workspace that does not contain
`EGO_HOME`.
Operator config commands run outside
the model's process sandbox and retain their existing atomic transaction.

## Executable content checks

Before ego runs a program it records the identity of every executable image the
call names: the resolved path, the file identity (device, inode, size, mode and
times) and a SHA-256 of the content. It checks that record again twice before
the launch.

- **You approve the call, or ego reuses your approval.** This covers an answer
  you give for one call, a session approval that ego reuses for an identical
  call, and a command that you type in the terminal shell. The check made right
  after the approval compares the resolved path and the file identity but does
  not read the content again. A one-shot answer covers that one call only.
- **A stored rule or cached judge decision allows the call.** Both checks
  compare the content hash. A replaced executable no longer matches, whatever
  its timestamps say.

The check at the launch reads and hashes the content for every authority, so a
changed image is refused in all cases.

The working directory, the resolved target, the arguments and the exact
environment are checked in both cases. If an image changed while a prompt was
open, ego asks again once with the new identity. Recovery from a refusal: run
the command again so that ego records the current image.

Measured on a 64 MiB image (remote build host, debug build): one content hash takes about
185 ms. Before this change an approved one-shot call hashed three times
(capture, revalidation after your answer, launch); it now hashes twice.
Details: `ARCHITECTURE.md`.

## Environment secret masking

Ego masks the values of environment variables that look like secrets (`[REDACTED]`)
in tool output, evidence, exports and status. A variable qualifies when one of its
`_`-separated name segments, compared case-insensitively, ends with `TOKEN`, `KEY`,
`SECRET` or `PASSWORD` (`API_KEY`, `github_token`, `PGPASSWORD`, `NGROK_AUTHTOKEN`),
or when it is `AWS_ACCESS_KEY_ID` or
`GOOGLE_APPLICATION_CREDENTIALS`. `MY_KEYBOARD` and `TOKENIZER` do not qualify.
A qualifying value shorter than 8 bytes is not masked (7 bytes stays visible, 8 is
masked), so a short value such as `it` cannot corrupt ordinary output. Qualifying
values are still masked across output chunk boundaries. Child environment
inheritance is unchanged. To mask a short value, use a value of 8 or more bytes.
