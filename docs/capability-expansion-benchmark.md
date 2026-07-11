# Vera Harness capability benchmark

Status: alpha snapshot, 2026-07-11

Run `sh scripts/benchmark-capabilities.sh` from the repository root after each
phase gate. The script measures the release binary, warm startup, host-reported
resident memory, registered-tool schema size, and the four-agent concurrency
fixture.

## Current snapshot

The final local verification run reported:

| Metric | Result |
| --- | ---: |
| Release binary | 4,365,840 bytes |
| Warm `vera --version` startup | 0 ms |
| Tool schemas | 1,358 approximate tokens / 5,487 bytes |
| Four-agent fixture | 0.38 s wall time |
| Maximum resident set size | unavailable in the managed sandbox |

An escalated macOS measurement of the same release shape reported 6,946,816
bytes maximum resident set size. A pre-expansion baseline was not captured
because this worktree already contained the alpha capability changes; future
phase snapshots should compare against this record rather than inventing a
historical baseline.

The release script also runs formatting, Clippy, the complete test suite, and
the Apple Silicon release build. Live provider and official-server smoke tests
remain environment-dependent and must use credentials supplied only to the
parent process.
