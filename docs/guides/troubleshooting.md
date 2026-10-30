# Troubleshooting

Start with the refusal and its owning boundary. A missing credential, unsupported
model fact, denied effect, and unavailable confinement are different problems;
changing one cannot safely be assumed to solve another.

```sh
ego doctor --json
ego config ls --effective --json
ego models --provider openai --json
```

Doctor reads one redacted ordered health snapshot. `ok`, `warning`, `error`, and
`not_checked` remain distinct. Root exit 1 means an operational error, not every
warning. Inspection does not log in, refresh a catalogue, reload extensions, edit
config, upload logs, or call a hosted provider. Confinement probes and provider
refresh can remain `not_checked` with the command/reason that would establish them.

| Symptom | Next check |
| --- | --- |
| No model selected in a headless run | Choose an available full slug explicitly; headless startup never persists a guess |
| Unknown provider/model or retired slug | Check the current catalogue/source; a provider mentioned in research may not be selectable |
| Unknown/unsupported effort | Use a supported explicit `--thinking`; absence is not permission to invent `medium` |
| Missing credential / 401 / 403 | Check the named profile's credential source and entitlement; do not copy tokens into diagnostics |
| No pricing quote under a money ceiling | Supply verified per-token facts or choose a token-only bound; unknown is not free |
| Project settings ignored | Inspect trust/bare posture; headless is untrusted by default |
| Denied tool despite an allow | Check every prepared operation, immutable denies, control-path rules, and origin of the allow |
| `perimeter-probe-temp-overlap` | TMPDIR must be outside every granted root for the negative probe. Inspect roots; an explicit user `ego config set 'roots=[]'` selects workspace-only access when that is the intended authority |
| Sandbox unavailable | Check host/backend evidence; do not claim confinement by changing the approval mode |
| Read-only root beneath writable grant | Inspect backend evidence: Linux Landlock child grants cannot subtract writable ancestor authority; built-in tools retain the read-only restriction |
| MCP fails or persisted session is quarantined | Inspect exact admitted declaration, authentication and retained failure; reload does not prove no prior effect |
| Invalid skill/hook candidate | Inspect exact diagnostics; last-good catalogue remains active |
| Schema exhaustion | Inspect final result/error and retry bound; exit 1 is intentional |
| Question in headless output | Resume the exact session with the matching question ID/JSON answer |
| Busy/paused/uncertain session | Inspect lifecycle status and evidence; use the owning continuation, not a new request to hide it |
| Context overflow | Compact explicitly; it is not a blind provider retry |

## `Risk: unknown - runs a program`

The analyzer has not established bounded effects. A command that only reads
files can still need approval, especially inside `bash -c` or another interpreter.
Use `[d]` to inspect the exact argv and remember preview. `[y]` permits the call;
`[a]` saves the displayed rules, not a grant for all commands of that program.
Shell scripts and absolute program paths normally stay exact. A changed script
can ask again. Inspect with `ego rules ls --json`, `ego rules explain`, and
`ego rules remember`; see [permissions](permissions.md) for syntax and the
optional `permissions.judge` in an `auto` profile. Changing approval policy does
not repair a sandbox denial.

## `Operation not permitted`

On macOS with `--sandbox workspace`, this can mean a read or exec path is outside
the admitted perimeter. Required instruction files outside the workspace are not
automatically admitted just because an instruction references them. Ask the user
for the required paths; do not silently skip mandatory instructions or try another
transport to bypass the block.

In a separate shell, inspect the new resolution with `ego config ls --effective`
and `ego doctor`; inside ego, inspect the captured one with `/config perimeter`.
For a non-secret instruction file, the user can add its canonical path with
`ego roots add /absolute/path/to/file --access read`. Declare each additional path explicitly. Saved changes require exit and resume; they do not apply live.

For `execvp ... Operation not permitted`, a read root is insufficient. Add the
canonical executable directory to `exec_paths` with `ego config edit`. Include
symlink targets and programs launched by a wrapper, not only the wrapper itself.
Keep `sandbox="workspace"`; switching to `off` removes filesystem confinement
rather than fixing the missing grant. System ceilings can prevent this recovery.

## `cargo: command not found`

This was observed when macOS confinement hid the Cargo toolchain outside the
workspace. It can also mean cargo is not installed or is absent from PATH. Check
command resolution in the user's shell first. If it resolves there but not inside
ego, inspect the perimeter. A read/read-write root does not grant exec.

Use `ego config edit` and extend the user or selected profile's `exec_paths` array
with the existing canonical tool directories. For a rustup installation,
these can include `~/.cargo/bin`'s resolved
target directory, and the selected `~/.rustup/toolchains/...` directory. Use actual
absolute paths, not literal `~` strings. Grant separate read roots for toolchain
resources and read-write roots for the caches/build outputs the task needs.
No toolchain preset or automatic PATH grant is available in the observed build.
Exit and resume, then ask ego to retry `cargo --version` before a build.

## `fatal: not a git repository: (null)`

In a linked git worktree, `.git` is a file that points to metadata under the main
repository's common git directory. That directory can be outside the admitted
workspace. The macOS sandbox then prevents git from reading it.

In an ordinary user shell at the worktree, run `git rev-parse --git-common-dir`
and resolve the output to an absolute canonical path. Inspect effective roots:
if the directory is already covered by an explicit root, no new grant is needed. Otherwise,
add that directory with `ego roots add /absolute/common/git/dir --access read`
for inspection, or `--access read-write` for git operations that update metadata.
Preserve required default roots as described above. Exit and resume to apply it.
This is a manual recovery; ego does not automatically add linked-worktree metadata.

## Git fails while selecting the macOS developer tools

The live notes identify `/var/select/developer_dir` as a required git read but do
not retain the full Xcode error text. Do not assume every Xcode error is a sandbox
failure. In the user's shell, run `xcode-select -p` and verify git works there.

If it works outside ego but fails under the workspace sandbox, add a read root
for the existing canonical `/var/select` path (macOS commonly resolves it under
`/private/var/select`). Grant the selected developer-tools directory for read and
exec: for example, an existing `/Library/Developer/CommandLineTools` directory
belongs in `exec_paths` when git needs its executables. Use `ego config edit`,
retain existing entries, and respect system ceilings. Exit and resume. A read root
alone does not authorize a tool launched from the developer-tools directory.
The exact Xcode diagnostic was not captured in the E2E; this entry does not invent it.

## `stopped: permission unresolved and nobody could be asked`

`ego resume ID "follow-up"` is headless even on a terminal. A required human
approval cannot be answered there. Run `ego resume ID` without a positional prompt,
then enter the follow-up in the composer. Include explicit model/mode/sandbox
flags if you need to override stored defaults. The E2E also observed staircase
history rendering on interactive resume; that display defect is not an approval
or perimeter change.

## `is not a provider/model slug, e.g. ollama/qwen3`

The status line can display only the model part, such as `gpt-6.1-sol`.
`--model gpt-6.1-sol` is invalid. Use the full selectable slug, such as
`--model openai-codex/gpt-6.1-sol`, and check `ego models` for availability.
A configured model can override the model saved with a resumed session; set the
full slug explicitly when you need the original selection.

## `missing_credential` / `run ego auth login anthropic`

Ego does not read the repository `.env`. Make `ANTHROPIC_API_KEY` available in
ego's process environment through your secret-management workflow, or use the
supported configured credential source. Follow the credential source named by the current diagnostic. Do not publish credentials.
An available key is not proof of sufficient credit or model entitlement.

## `no capabilities are configured`

A `permissions.judge` slug can be accepted by `ego config set` but refused at
launch when its capability facts are absent. Config mutation validates the typed
value, not catalogue availability. The E2E hit this with
`openai-codex/gpt-6-luna`; do not use that slug as a working sentinel example.
Select a model with supplied facts and credentials (check `ego models`), or add
verified facts through the documented [model configuration](../reference/configuration.md).
Do not invent capability values. The judge receives bounded task context and action evidence; same-provider
judges receive raw argv and cross-provider judges receive redacted structure.
It can still escalate. See [judge action evidence](permissions.md#judge-action-evidence).

## `unable to access ~/.config/git/ignore: Operation not permitted`

The historical E2E observed this git warning with an explicit shared-tree perimeter.
The actual path can be expanded to the user's absolute home path. Git reads
configuration and ignore files outside the workspace. Grant the existing
non-secret file with `ego roots add /absolute/path/to/.config/git/ignore --access read`,
preserve required default roots, and exit/resume to apply it. Other configured git
files can need their own read grants. Homebrew git ran where `/usr/bin/git` failed
in this run, but changing executables is not a substitute for required grants.

## `credit balance is too low`

This can be a real Anthropic credit or entitlement failure. It can also be caused
by non-secret identity headers in `[auth.anthropic.headers]`. In the 2026-10-10
live check, the same valid `ANTHROPIC_API_KEY` worked in a request without the
old Claude Code identity headers and failed with those headers. The operator
removed the header overrides. Inspect your own user configuration without
publishing credentials; remove inappropriate identity overrides with
`ego config edit`, then restart. This observation does not prove every balance
error is a header error. The header schema still applies; forbidden wire headers
are not valid overrides. See [authentication keys](../reference/configuration.md#authentication-keys).

## Other live E2E limitations

The 2026-10-10 run observed root help without the run options. Root help now
lists the shared run options. The trust offer names absolute file paths and
explains both answers; trust admits project files and their configuration.
`ego roots` uses plain receipts and access/path listings. Its edits remind the
user to exit and resume because running sessions keep their captured perimeter.
The `ego config set` receipt still describes the stored change, not a live
session reload.

The run also observed answered approval blocks remaining in scrollback, slow
exec calls, and a configured MCP server unavailable without a visible warning.
Startup now attaches every enabled configured server and reports each one that
admission refuses or that fails to start, once, with its sanitized cause.
Inspect configuration with `ego mcp ls`. Set `enabled = false` in the entry to
detach a server; `--mcp NAME` still attaches it for one run.
One auto-mode access check later took 4 minutes 29 seconds.
Exec latency in that run was not isolated to a proven layer; do not treat sandbox
startup cost as an established cause. Use the built-in read/grep tools for file
inspection when available. They still enforce their own read authority.

Do not implement the report's proposed full-disk read/exec perimeter, toolchain
autodiscovery, inline root grants, or learned sentinel rules as troubleshooting
steps: they were proposals, not evidence of shipped behavior.

## Platforms

macOS and Linux have OS-specific confinement adapters; guarantees require host
capability evidence and the actual admitted production path. Windows listener,
process ownership, and private-state support exist, but Windows filesystem/network
confinement remain unsupported. An unconfined acknowledgement does not establish confinement.

## Diagnostics

`EGO_LOG` selects tracing directives and `EGO_LOG_FILE` an explicit destination.
Diagnostics remain off JSON/JSONL and served protocol stdout. Body logging is a
separate sensitive opt-in (`ego::body`); ordinary diagnostic levels do not make
request/response bodies safe to publish. Keep local exports and private credential
stores out of shared logs. Use a sanitized export and review its remaining prose
before publication.

To compare provider request prefixes, set `EGO_LOG=ego::body=debug` and
`EGO_LOG_FILE` to a private local file. Each `provider request` entry contains
the JSON body and API path, without HTTP headers. Prompt text and tool results
remain sensitive. Codex requests use one lineage routing key in both
`prompt_cache_key` and the `session-id` header; `cache = 'none'` omits both.

Source: [doctor](../../crates/ego-cli/src/doctor.rs),
[diagnostics](../../crates/ego-cli/src/diagnostics.rs),
[confinement diagnostics](../../crates/ego-cli/src/confinement_diagnostics.rs).
