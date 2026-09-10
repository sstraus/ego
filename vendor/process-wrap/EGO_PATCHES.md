# Ego patches

## Baseline

- Package: `process-wrap` 10.0.0
- Published archive:
  `https://static.crates.io/crates/process-wrap/process-wrap-10.0.0.crate`
- Archive SHA-256:
  `0e3f4237d0e4741eb50bc5584db701f1299c85fa31ff0274dd6445e79dc42d12`
- Upstream repository: `https://github.com/watchexec/process-wrap`
- Published Git revision: `d02486b8d1e393a4ac27023d591b6f35c6417ffb`

The published archive was extracted without changing its source or license
files. `tests/job_object_completion.rs` is an ego-owned contract test
and is not part of the published archive. Because the normalized upstream
manifest disables automatic test discovery, the only local change to a
published baseline file is this explicit registration in `Cargo.toml`:

```toml
[[test]]
name = "job_object_completion"
path = "tests/job_object_completion.rs"
```

This baseline contains no semantic correction. In particular, it does not yet
implement the `ACTIVE_PROCESS_ZERO` completion correction required by story
110.
