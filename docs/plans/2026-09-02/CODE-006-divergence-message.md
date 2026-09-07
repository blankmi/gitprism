# Plan CODE-006 — dest→source divergence message names the right side

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, CODE-006 |
| Severity / priority | LOW / P3 |
| Effort | Small |
| Decision required | No — wording only; decision 0038's intent (name the branch, say the sides diverged, hand reconciliation to the operator without naming a strategy) is kept |
| Depends on | — |
| Status | Proposed |

## Problem

`divergence_after_exhausted_retries_message(branch, ff_target)`
(`src/commands/sync/mod.rs:115-119`) always prints "dest branch {branch} and
source have diverged". Called with `"source"` at `:1579` for dest→source, the
lost race is against source's own remote, and dest is not involved.

## Steps

### Step 1 — failing test

**Files.** `src/commands/sync/tests/mod.rs` (or wherever the existing message
test lives; grep `divergence_after_exhausted_retries_message`).

**Test first.** For the dest→source variant, assert the message contains
"source branch" and "source's remote" and does not contain "dest branch".

### Step 2 — parameterize the sentence

**Files.** `src/commands/sync/mod.rs:115-119` and both call sites.

**Change.** Replace the `&str` parameter with `enum FastForwardTarget { Dest,
Source }` and write two sentences:

* `Dest`: unchanged text.
* `Source`: `gitprism sync: source branch {branch:?} moved on its remote while
  gitprism was reflecting dest's commits — {MAX_RACE_RETRIES} refetch-and-recompute
  attempts still couldn't fast-forward it; reconcile the local branch with the
  configured source remote using ordinary git, then rerun gitprism sync`.

Keep the rule from the doc comment: never mention merge, rebase, or cherry-pick.

## Verification

`cargo test divergence`; fmt and clippy clean.
