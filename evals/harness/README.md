# Harness evaluation

Compares a candidate `ego` binary with a base binary on a frozen task suite.
Information only: no gate. Procedure: `docs/maintaining.md`, section "Evaluate a
performance-relevant change". Commands: `affected`, `verify-suite`, `run`,
`decide`, `record` in `harness.py`. Standard-library Python only.

## Add a task

1. Create `tasks/<id>/` with `task.toml`, `workspace/`, `hidden/` and `reference/`.
   - `task.toml`: `id`, `difficulty` (`medium` | `hard`), `kind = "coding"`,
     `editable` (every other changed file is a forbidden edit), `prompt`,
     `hidden` (path of the hidden test file) and `leakage` (when and how the task
     was written; never derived from an agent run).
   - `workspace/` is what the agent sees: `TASK.md`, a stub or buggy source, and a
     visible test that does not prove the task.
   - `hidden/` holds unittest files. Every hidden test must fail on the untouched
     workspace. `reference/` holds the solved versions of the `editable` files.
2. Add a trap that a quick self-check misses.
3. Run `python3 -B evals/harness/harness.py verify-suite --update-digests`, then
   review the new digests. Any later fixture byte change needs this step again.
4. A change to a frozen task invalidates earlier records: say so in the record.

## Add a `tool_use` case (future)

`kind = "coding"` is the only kind the runner scores. A `tool_use` case is
reproduced from a real agent mistake. It adds a `kind = "tool_use"` task whose
hidden check reads the recorded `tool_calls` of `run.json` (for example "never
`exec` before `read`") instead of running a test file. Add the scorer to
`runner.py` when the first case exists; do not add it earlier.
