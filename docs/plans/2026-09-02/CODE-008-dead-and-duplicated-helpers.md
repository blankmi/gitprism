# Plan CODE-008 — dead and duplicated helpers

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, informational CODE-008 |
| Severity / priority | INFO / P3 |
| Effort | Small; four independent commits |
| Decision required | No, except item D which is deliberately *not* changed |
| Depends on | — |
| Status | Proposed |

## Items

### A — dead `.stdout(Stdio::null())` in `remote_ref_exists`

**File.** `src/git.rs:553`.

`run_git_output` pipes stdout for capture, so the earlier `.stdout(null)` is
overwritten. Remove the line. Behaviour is unchanged; the captured `ls-remote`
line is bounded by `SMALL_OUTPUT`. Existing `remote_ref_exists` tests cover it.

### B — two hex codecs

**Files.** `src/marker.rs:102-125` (`hex_encode`, `hex_decode_mac`),
`src/commands/resolve.rs:797-820` (`hex_encode`, `hex_decode`).

**Test first.** Unit tests for the shared functions: round trip, odd length
rejected, non-hex rejected, uppercase accepted.

**Change.** New `src/hex.rs` with `pub(crate) fn encode(&[u8]) -> String` and
`pub(crate) fn decode(&str) -> Option<Vec<u8>>`. `marker` converts to
`[u8; MAC_BYTES]` with `try_into`. Delete both local copies.

### C — `with_recovery_failures` lives in `setup`

**Files.** `src/commands/setup.rs:546`, `src/commands/resolve.rs:474, 489, 506`.

**Change.** Move to `src/commands/mod.rs` (or `src/recovery.rs`) as
`pub(crate) fn with_recovery_failures`. Update both callers. Its tests move with
it.

### D — `Resolve-Dest-Ref-Existed` is always `true`

**Files.** `src/commands/resolve.rs:420, 1006, 1105`.

`parse_operation_state` requires exactly ten fields, so dropping the field is a
state-format change (decision 0027) with a version bump and a migration for
in-flight resolutions. Not worth it. Instead: add a comment at the writer
explaining that since decision 0039 a mirror-only branch with no dest ref is
refused before a resolution starts, so the field is constant and retained for
format stability; keep the reader's `== "true"` check as a tamper guard.

## Verification

`cargo test`; fmt and clippy clean; `grep -n 'fn hex_' src` shows one module.
