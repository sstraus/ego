# Coordinator response fixtures

`sessions.json` and `inbox.json` retain the response shapes captured from
TUICommander. `children.tsv` retains matching child identifiers and ordering.
Personal paths, repository names, descriptions, and work labels are replaced
with example values. Identifiers and aliases are synthetic and consistent across response joins.
Optional fields, activity states, row order, and tracked/untracked distinctions
retain the recorded response shape; timestamps are synthetic.

Tests compare tracked children with live session rows and enumerate unread
result files with known filesystem timestamps. These are adapter fixtures,
not evidence that a public installation has the original services or state.
