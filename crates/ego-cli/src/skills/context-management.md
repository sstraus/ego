# Manage context at a safe boundary

Use the context tool only when it is present in your tool catalogue. A host
without a durable successor owner does not offer it.

Prefer compact after a finished sub-task, high measured context usage, or a
large topic change that still needs prior decisions and evidence. It asks the
current model for the canonical checkpoint and continues in a clean session.
The automatic pressure path remains available without a tool request.

Use clear only with explicit operator enablement and an active session goal.
It discards the conversation and earlier checkpoints. Only the active goal,
its binding ceilings, fresh system/project instructions, and your short
handoff remain. Write every still-needed constraint, unresolved question,
reference, verified result, and next action in that handoff. Do not clear if
you cannot state them safely. Clear is disabled by default.

Do not request either action with unfinished tool effects, a running detached
process, a pending question, or pending operator input. Finish and verify the
sub-task first. Never use a transition to bypass a refusal, forget a blocker,
reset a budget, or claim that work is complete.

Call context with action (compact or clear), a specific reason, and a short
handoff. Do not combine it with a question or a background launch. The whole
tool batch must finish before the idle owner can switch sessions. The request
and reason are model-authored, not operator instructions. Work continues
without a new operator prompt after the target is published.

The default cooldown is ten completed work turns after a transition. Do not
retry a refused request in a loop. The operator can set --context-interval N,
disable the tool with --no-context-tool, or enable clear with --context-clear.
