# Plan CODE-005 — setup rollback never strands HEAD on the scratch ref

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, CODE-005 |
| Severity / priority | LOW / P3 — reclassified as defensive cleanup, see below |
| Effort | Small |
| Decision required | No |
| Depends on | — |
| Status | Proposed |

## Problem, as reclassified

`rollback_branches` (`src/commands/setup.rs:620-671`) moves HEAD to
`refs/heads/gitprism-setup-rollback-scratch` unconditionally (`:626`) but
restores it only when `original_head` is `Some` (`:663-669`). If rollback ever
ran with `original_head == None` and a readable HEAD, HEAD would be left on a
branch that does not exist.

That state is not reachable through `setup::run` today. `original_head`
(`:74-77`) is `None` only when `HEAD` cannot be read or is not symbolic. A
detached HEAD is refused at `:137-144` before any mutation, and an unreadable
`HEAD` file makes that same `head_detached()` call fail, also before any
mutation. The review's wording "can leave HEAD on the scratch ref" overstated
this; the review document carries an erratum. What remains is a function whose
contract allows a caller state it does not handle, which is worth closing so a
future caller cannot trip it.

## Steps

### Step 1 — failing unit test, labelled as defensive

**Files.** `src/commands/setup.rs` tests, next to
`rollback_collects_ref_lookup_and_head_failures` (`:1517`).

**Test first.** Call `rollback_branches(&repo, &touched, None)` on a repo whose
HEAD points at `refs/heads/main`. Assert afterwards that HEAD's symbolic target
is still `refs/heads/main` and that the returned failures list says HEAD was
not moved because its original target was unknown. The test's doc comment
states that `setup::run` cannot reach this state and why the guard exists
anyway.

### Step 2 — guard the move

**Files.** `src/commands/setup.rs:626`.

**Change.** Move HEAD to the scratch ref only when `original_head.is_some()`.
When it is `None`, push one informative entry into `failures` ("HEAD's original
target could not be read before setup; branches were rolled back without moving
HEAD — verify HEAD manually") and proceed with the branch rollback. If a touched
branch is the one HEAD points at, libgit2's refusal surfaces as the existing
per-branch failure entry, which is the correct fail-closed outcome.

### Step 3 — regression for the normal path

The existing end-to-end rollback tests
(`run_rolls_back_created_branches_when_a_later_branch_fails_to_commit`, `:1710`,
and `run_rolls_back_a_pre_existing_branch_to_its_original_tip_when_a_later_branch_fails`,
`:2290`) already inject a commit-phase failure after one branch was mutated.
Extend one of them to assert HEAD is restored and the scratch ref is gone, so
step 2 did not change the common case.

## Verification

`cargo test setup`; fmt and clippy clean.

## Revision history

* 2026-09-02, after plan review: reclassified from a correctness fix to
  defensive cleanup after confirming that `setup::run` rejects detached and
  unreadable HEAD before mutation. Step 3 now points at the existing
  commit-phase injection tests instead of a new fetch-phase test, which could
  not exercise rollback (see TEST-GAPS step D).
