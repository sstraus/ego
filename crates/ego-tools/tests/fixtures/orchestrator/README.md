# Recorded coordinator reads

`sessions.json` records the real TUIC `session action=list` response seen by
example-coordinator on 2026-10-05: 18 rows. Remote `state` retains only shell state and
activity time; prompt and question text were removed because they are not read
by this adapter. Other response fields are retained.

`children.tsv` is a byte copy of `~/Gits/.tmp/coordinator/children.tsv` from the
same work session. Six tracked IDs are live in the recording. The independent
reference is `orchestrator/tools/coord-resume.sh`'s Children section: iterate
the TSV in order, select `.session_id == sid`, and omit absent sessions.
The Unread section uses `find results -maxdepth 1 -name '*.md' -newer stamp`;
the integration test runs that real command over files with known OS mtimes.

The original 30-row criterion was replaced on coordinator authority. No
sessions were fabricated to reach that number.
