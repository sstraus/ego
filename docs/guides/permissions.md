# Permissions and confinement

Permissions decide which prepared effects may run. Confinement limits what the
operating system allows an executor to do. Workspace trust decides whether project
configuration and instructions are admitted. These are separate authorities.

## Permission modes

| Mode | Purpose |
| --- | --- |
| `plan` | Prevent mutating operations; inspect and reason |
| `default` | Apply ordinary deterministic policy and escalation |
| `auto` | Use deterministic low/bounded analysis, a configured judge where needed, then ordinary escalation |
| `edits` | Allow ordinary editing while retaining separate checks for higher-risk effects |
| `yolo` | Relax ordinary session approval; retain system denies, confinement, offline, budgets, and hard boundaries |

Trusted interactive sessions choose `auto` only when no mode is configured.
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
with `--yes`. Interactive remembered permissions use the same transaction.
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

Adaptive is the fresh-config default, independently of permission mode `auto`.
Explicit `workspace`, `ro`, and `off` selections retain their behavior; no config
file is rewritten. Select those levels when their stricter or open reach is
required. Adaptive
currently requires macOS Seatbelt and successful native capability probes;
unsupported platforms fail admission with explicit alternatives. It captures
operator `CARGO_HOME`, `RUSTUP_HOME`, trusted mbx `cache_dir`, validated git common
and linked-worktree metadata, `TMPDIR`, and an operator `CARGO_TARGET_DIR` before
commands. Cargo/rustup defaults apply when their environment variables are absent.
Those roots appear as `derived` in perimeter inspection, with no per-cache judge.
Configured ceilings and readonly roots remain binding. Shared-cache writes are
intentional. Workspace and derived toolchain/target executables remain runnable.

Adaptive's OS credential floors cannot be lifted by permission rules or human
sandbox grants: SSH/AWS/GnuPG, netrc, keychain/browser stores, provider/ego/Cargo
credentials and `.env*` outside the workspace. Git config/hooks and worktree
selectors stay protected against writes. Ordinary OS privacy restrictions also
remain. These OS floors are separate from the default read rules described above.
Report the exact refused target and its task relevance. A failed mutation can
leave partial effects; do not assume replay is safe. Automatic residual-write
judgment is pending the shared judge integration; the existing human grant flow
still handles eligible refusals. A future judge-write grant will also forbid
execution within that residual root for the rest of its session.

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
