# Plan PERF-002 — memoize filtered subtrees within one replay

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, PERF-002 |
| Severity / priority | LOW / P2 |
| Effort | Small |
| Decision required | No — pure cache with deterministic inputs and a static cap; note in `log.md` and add the new limit to decision 0032's list |
| Depends on | ARCH-001 (so resolve gets the memo through the shared loop, not a second one) |
| Status | Proposed |

## Problem

`filter_tree` (`src/commands/sync/filter.rs:28-108`) walks and rewrites the whole
tree twice per pending commit (base and theirs). Consecutive commits share almost
every subtree, so a backfill of thousands of commits on a large monorepo costs
O(commits × tree size).

## Correctness basis

The filtered result of a subtree depends only on `(subtree oid, path prefix,
exclude list)`. Within one `build_pending_dest_tip` call the exclude list is
fixed: decision 0026 supplies one verified exclude list for the run, and
0037's pre-pass, as amended by 0042, refuses pending `.gitprismignore`
mismatches. Decision 0036's branch-additive exclusions were superseded by
0037. A memo keyed on `(Oid, PathBuf)` scoped to that call is exact; an OID-only
key, as suggested in the original review, would be incorrect because exclusion
matching depends on the path prefix.

## Steps

### Step 1 — failing test

**Files.** `src/commands/sync/tests/filter.rs`.

**Test first.** Build tree `T1` with a 200-entry subtree `big/` and a file
`top.txt`; `T2` = `T1` with `top.txt` changed. Filter both through one memo.
Assert the memo reports one miss and one hit for `big/`, and that the two results
equal the unmemoized results byte-for-byte (compare tree oids). A second test:
the same subtree oid under two different prefixes (`a/big`, `b/big`) with an
exclude pattern anchored to `a/big/secret` produces two different outputs and two
misses.

### Step 2 — `FilterMemo`

**Files.** `src/commands/sync/filter.rs`.

**Change.** `pub(crate) struct FilterMemo { by_input: HashMap<(Oid, PathBuf), Option<Oid>> }`
(`None` = filtered to empty). `filter_tree(repo, tree, prefix, exclude_list,
memo: &mut FilterMemo)`; check the memo before recursing into a subtree, insert
after. Test-only hit/miss counters. `TraversalBudget` semantics: a memo hit
counts as one visit (the entry), not the subtree's contents, which only lowers
the count.

**Bound.** The memo lives for a whole replay of up to `MAX_PENDING_COMMITS`
commits and its keys come from repository-controlled trees, so it needs a
static limit like every other repository-driven collection (decision 0032).
Add `limits::MAX_FILTER_MEMO_ENTRIES` (proposal: 250,000; one entry is an
`Oid`, a `PathBuf`, and an `Option<Oid>`, so the cap is tens of megabytes at
most). Once full, `insert` becomes a no-op and filtering proceeds uncached;
lookups still hit for what is already stored. No eviction: it keeps the memo
deterministic and the behaviour trivially correct. Test: fill the memo to the
cap with synthetic keys, filter one more distinct subtree, assert the entry
count did not grow and the result equals the unmemoized result.

### Step 3 — thread it

**Files.** `src/commands/sync/mod.rs` (`build_pending_dest_tip`,
`build_pending_source_tip` if it filters), `src/commands/sync/policy_check.rs`
if it calls `filter_tree`, resolve via ARCH-001.

**Change.** One `FilterMemo::default()` per build call; pass `&mut memo` to
every `filter_tree` inside the loop. No memo crosses a branch boundary.

### Step 4 — measure

Extend the step 1 test into an end-to-end sync of 50 pending commits on a tree
with 1,000 files and assert the miss count is proportional to changed subtrees,
not to `50 × 1,000`.

## Verification

`cargo test filter`, full suite; fmt and clippy clean.
