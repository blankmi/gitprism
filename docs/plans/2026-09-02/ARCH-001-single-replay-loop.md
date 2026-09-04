# Plan ARCH-001 — one source→dest replay loop for sync and resolve

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 4, ARCH-001 |
| Severity / priority | MEDIUM / P1 |
| Effort | Small |
| Decision required | No — structural; decision 0016's "sync and resolve must never disagree" is the existing rule being enforced |
| Depends on | CODE-002 step 1 (the parity test) |
| Status | Implemented 2026-09-03 (steps 1-5) |

## Problem

`resolve_source_to_dest`'s replay at `src/commands/resolve.rs:325-364` is a
hand copy of `build_pending_dest_tip` at `src/commands/sync/mod.rs:1066-1146`.
It has diverged twice (decision 0037 pre-pass, finding F-03; CODE-002). The two
loops already compute the same things: filtered base and theirs trees,
`git::merge_tree`, skip on unchanged tree, `build_dest_commit` on a clean merge,
stop at the first conflict.

## Target shape

Resolve calls `build_pending_dest_tip` and reads the result:

* conflict commit and paths: `build.conflict`;
* the dest commit the conflict sits on (resolve's `dest_base`):
  `build.new_tip.unwrap_or(dest_tip)` — identical to the loop's `parent` at the
  moment of the conflict, so no struct change is needed;
* `generated_mappings` is ignored by resolve (it has no mapping index to feed).

## Steps

### Step 1 — parity test

Land CODE-002 step 1 first. Add a second assertion set to it: with a clean
prefix of two commits before the conflict, resolve pushes exactly the dest
commits sync would have built (compare dest `feature`'s first-parent line after
`sync` halts against what `resolve` builds when started from the pre-sync state
in a second fixture).

### Step 2 — expose the sync function

**Files.** `src/commands/sync/mod.rs`.

**Change.** Make `build_pending_dest_tip`, `PendingDestBuild`, and `Conflict`
(fields `commit`, `paths`) `pub(crate)`. Keep the `#[allow(clippy::too_many_arguments)]`;
do not change the signature in this step.

### Step 3 — replace resolve's loop

**Files.** `src/commands/resolve.rs:309-369`.

**Change.**

* Keep the 0037 pre-pass at `:311-323` where it is (sync runs its own before
  calling `build_pending_dest_tip`; both operate on `pending_commits(repo,
  boundary, source_tip)`).
* Replace lines 325–364 with one call to `build_pending_dest_tip(repo, config,
  exclude_list, boundary, dest_tip, source_tip, source_root, branch, state_key)`.
* `let Some(conflict) = build.conflict else { bail!(no conflict) }`;
  `dest_base = build.new_tip.unwrap_or(dest_tip)`; the rest of the function
  (`if dest_base != dest_tip { push clean prefix }`) is unchanged.
* Remove imports that become unused (`filter_tree`, possibly `build_dest_commit`
  if `--continue` does not use it — check before deleting).

**Done when.** All resolve tests pass unchanged; the parity test passes.

### Step 4 — confirm dest→source has no equivalent duplication

`resolve.rs:1193` and `:1278` already call `pending_dest_commits` and
cherry-pick only `pending.first()`; there is no prefix build to unify. Record
that in the commit message so the next reader does not look for one.

### Step 5 — guard against re-divergence

**Files.** `src/commands/sync/mod.rs` doc comment on `build_pending_dest_tip`.

**Change.** State that resolve is a caller and that any new pre- or post-condition
belongs inside the function, not at a call site. Reference decision 0016.

## Verification

Release gates. Diff of `resolve.rs` should be net-negative by roughly 35 lines.
