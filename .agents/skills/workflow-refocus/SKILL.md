---
name: workflow-refocus
description: Reassess long-running engineering work after compaction, each active hour, repeated validation or handoff cycles, orchestration stalls, or a speed concern. Exclude ordinary implementation and code review.
---

# Workflow Refocus

## Preserve

- Scope
- Product behavior
- Architecture
- Acceptance criteria
- Evidence standards

## Inspect once

- Hook checkpoint first after compaction
- User outcome
- Active plan or story
- Recent commits
- Dirty-path ownership
- Live agents
- Current assignment identity and epoch; last semantic `RESULT` or `BLOCKED`
- Queued commands and pending composer; remaining one-shot authority
- Treat lifecycle notifications as state only and delivery acknowledgements as routing only, never semantic handoffs.
- Latest gate
- Exact blocker
- Never quote or commit the checkpoint wholesale
- Never reconstruct the transcript

## Diagnose one delay

- Delivery: accepted behavior, integrated code, green evidence, closed decisions
- Cost: spawns, payload, handoffs, waits, repeated reads or gates, premature commits
- Select the largest avoidable delay
- Cite evidence
- Label inference

## Correct once

- Close completed sessions with no related reuse.
- Reuse related sessions.
- Never assign into an ambiguous or non-drained queue.
- Use event-driven semantic handoffs without polling when agents notify the root.
- Consume one-shot external, billable, or destructive authority immediately after the action, regardless of outcome.
- Never retry a one-shot action from a stale or ambiguous handoff; require renewed authority once consumed.
- Never spawn a refocus or microtask worker.
- Dispatch all ready independent batches together.
- Give Luna one validation and fixture-repair batch.
- Inspect every failure in the batch before one focused rerun.
- Commit one green slice before dependent work.
- Prompt only: objective, ownership, exclusions, evidence, handoff.

## Persist

- Update the plan only when the next action changes.
- Store only:

  - Outcome and next action
  - Durable commits and exact gate evidence
  - Dirty ownership and unrelated changes
  - Reusable agent IDs and lifecycle
  - Current assignment identity and epoch, semantic handoff cursor, and authority remaining
  - Unresolved blocker or decision

## Return

`REFOCUS`: evidence; bottleneck; correction; next stable point; authority remaining.

Resume work.
