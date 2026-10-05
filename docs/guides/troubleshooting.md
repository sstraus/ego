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
| Read-only root beneath writable grant | Fix overlapping canonical roots; a backend cannot subtract the nested read access |
| MCP fails or persisted session is quarantined | Inspect exact admitted declaration, authentication and retained failure; reload does not prove no prior effect |
| Invalid skill/hook candidate | Inspect exact diagnostics; last-good catalogue remains active |
| Schema exhaustion | Inspect final result/error and retry bound; exit 1 is intentional |
| Question in headless output | Resume the exact session with the matching question ID/JSON answer |
| Busy/paused/uncertain session | Inspect lifecycle status and evidence; use the owning continuation, not a new request to hide it |
| Context overflow | Compact explicitly; it is not a blind provider retry |

## Platforms

macOS and Linux have OS-specific confinement adapters; guarantees require host
capability evidence and the actual admitted production path. Windows listener,
process ownership, and private-state support exist, but Windows filesystem/network
confinement and the final cross-platform matrix remain open work. An unconfined
acknowledgement is not proof of confinement. This documentation task verifies
its own macOS build and does not claim native Windows/Linux execution.

## Diagnostics

`EGO_LOG` selects tracing directives and `EGO_LOG_FILE` an explicit destination.
Diagnostics remain off JSON/JSONL and served protocol stdout. Body logging is a
separate sensitive opt-in (`ego::body`); ordinary diagnostic levels do not make
request/response bodies safe to publish. Keep local exports and private credential
stores out of shared logs. Use a sanitized export and review its remaining prose
before publication.

Source: [doctor](../../crates/ego-cli/src/doctor.rs),
[diagnostics](../../crates/ego-cli/src/diagnostics.rs),
[confinement diagnostics](../../crates/ego-cli/src/confinement_diagnostics.rs), and
[platform research](../internals/22-windows-runtime-requirements.md).
