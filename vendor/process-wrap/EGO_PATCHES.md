# Ego patches

## Baseline

- Package: `process-wrap` 10.0.0
- Published archive:
  `https://static.crates.io/crates/process-wrap/process-wrap-10.0.0.crate`
- Archive SHA-256:
  `0e3f4237d0e4741eb50bc5584db701f1299c85fa31ff0274dd6445e79dc42d12`
- Upstream repository: `https://github.com/watchexec/process-wrap`
- Published Git revision: `d02486b8d1e393a4ac27023d591b6f35c6417ffb`

The published archive was extracted without changing its license files.
`tests/job_object_completion.rs` is an ego-owned contract test and is
not part of the published archive. Because the normalized upstream manifest
disables automatic test discovery, `Cargo.toml` registers it explicitly:

```toml
[[test]]
name = "job_object_completion"
path = "tests/job_object_completion.rs"
```

## Semantic correction

- `Cargo.toml` and `Cargo.toml.orig` add
  `windows/Win32_System_SystemServices` to `job-object` so the correction uses
  the official `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` binding.
- `src/windows.rs` associates the expected completion key with the port and
  validates it on every Job packet. It drains other notifications until
  `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO`, returns `Continue` only for a finite
  `WAIT_TIMEOUT`, propagates every other error, and applies one total finite
  deadline while clamping each native wait below `INFINITE`.
- `src/tokio/job_object.rs` caches the root exit status only after the Job-empty
  barrier succeeds. A cancellation-safe stored receiver preserves a background
  wait result across cancellation; `try_wait` remains pending until both root
  exit and `ACTIVE_PROCESS_ZERO`, then returns the stable cached status.

A failed barrier or cancelled caller wait never converts a root exit into
cached success. `start_kill` and all public APIs are unchanged. These are the
only semantic source changes from the published 10.0.0 baseline.
