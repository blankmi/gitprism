# Plan CODE-002 — resolve uses the same loop prevention as sync

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 4, CODE-002 |
| Severity / priority | MEDIUM / P1 |
| Effort | Small |
| Decision required | No — decision 0043 addendum (F-04) and 0046 Addendum 2 Finding L already settle the rule |
| Depends on | — |
| Superseded by | ARCH-001 step 3 removes this code path entirely; step 1 here is the interim fix and the lasting regression test |
| Status | Implemented 2026-09-03 through ARCH-001; shared replay and regression parity tests replace the interim predicate-only fix |

## Problem

`src/commands/resolve.rs:329-339` skips a pending source commit only when
`marker::verify(commit, branch, [Setup, DestToSource])` matches — branch-scoped.
Sync's `loop_prevented` (`src/commands/sync/mod.rs:1050-1063`) uses
`marker::verify_self`, so a `DestToSource` marker inherited from a sibling branch
is skipped by sync and replayed by resolve. The two commands disagree about which
commit is next, which decision 0016 exists to prevent.

## Steps

### Step 1 — failing parity test

**Files.** `src/commands/resolve.rs` tests module.

**Test first.** Fixture: round-tripped `main` imports a dest-native commit
(source gets `M(C0)` with `Gitprism-Branch: main`); branch `feature` is cut from
`main` after `M`; `feature` gains a source commit that conflicts with dest
`feature`'s content so a source→dest conflict exists. Run sync (halts on the
conflict, pushes the clean prefix). Then run
`resolve feature --direction source-to-dest`. Assert:

* the conflict it selects is the same source commit sync reported;
* dest `feature` has no commit whose `Gitprism-Source-Commit` is `M`.

Today the second assertion fails: resolve replays `M` as part of the clean prefix.

### Step 2 — use the shared predicate

**Files.** `src/commands/sync/mod.rs:1050`, `src/commands/resolve.rs:329-339`.

**Change.** Widen `loop_prevented` to `pub(crate)`. Replace the
`marker::verify(...)` block in resolve with `if loop_prevented(&source_commit,
state_key) { continue; }`.

**Done when.** Step 1 passes; existing resolve tests pass.

### Step 3 — fold into ARCH-001

When ARCH-001 replaces resolve's loop with `build_pending_dest_tip`, the step 1
test stays as the regression guard for the shared function.

## Verification

`cargo test` for `resolve` and `sync`; fmt and clippy clean.
