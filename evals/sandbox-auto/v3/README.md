# Sandbox policy fixtures

`cases.json` is consumed by the offline `ego-core` and `ego-tools` tests. It
covers routing, task-dependent residual writes, attacks, applicator faults,
error parsing, and configuration selection. Paths and metadata use example
identities. Routing cases normalize observed failure classes; they are not
exact syscall traces. Residual judgments are synthetic test scenarios.

`judge-system.txt` is included by the production residual-write judge. It
requires bounded evidence and rejects uncertainty or attempts to cross policy
floors. Keep it with the source: it is a runtime input, not a generated result.

These fixtures do not establish live model accuracy or native confinement.
