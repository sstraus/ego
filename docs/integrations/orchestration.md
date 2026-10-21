# Native orchestration

Native orchestration connects a coordinator session to TUICommander children.
Observation is the default: the model can inspect bounded child status and result
paths without sending instructions or creating processes.

When `TUIC_SOCKET` exists, the admitted catalogue can expose `orchestrator_status`
and `orchestrator_brief`. `orchestrator_active` is absent/refused in observation
mode. The operator explicitly opts in through base or profile
`orchestration.mode="active"`.

```toml
[profile.coordinator.orchestration]
mode = "active"
```

An invocation selects the profile with `--profile coordinator`. This is an
operator choice; documentation does not activate it.

Active requests are `spawn`, `say`, `consult`, `worktree_create`, and `land`.
They prepare exact guarded argv; a brief is never shell-evaluated. Consultations
have read-only authority. Landing uses the existing coordinator High decision
card, including under yolo, and a refused/unacknowledged submission stays visible.
Spawn receipts are durable tool-completion evidence. The ACP pump supervises only
successful native spawn/consult rows belonging to its root; it observes outside
model turns and bounds its idle resend with a durable cursor.

This surface depends on the operator's installed TUICommander/orchestrator tools.
Deterministic source tests do not establish live paid-agent spawning, a real
landing, or the outstanding multi-day observation proof.

Sources: [domain contract](../../crates/ego-core/src/orchestrator.rs),
[OS adapter](../../crates/ego-tools/src/orchestrator.rs),
[active operations](../../crates/ego-tools/src/orchestrator_active.rs).

`ego orchestrator brief [--root PATH]` provides the same deterministic child and
unread-path inspection without a model turn. ACP cards append `notice_delivered`
facts to the existing session stream after transport enqueue, fsyncing before the
pump acknowledges its batch. Those facts are independent of conversation turns;
transport acceptance does not establish host rendering. Measurement and live
comparison tooling: [observation README](../../ARCHITECTURE.md).
