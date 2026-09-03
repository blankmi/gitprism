# Plan CODE-001 — dest→source resumes from the branch's own boundary

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 3, CODE-001 |
| Severity / priority | HIGH / P0 |
| Effort | Medium |
| Decision required | Yes — new decision 0048, written before step 3 |
| Depends on | — |
| Unblocks | Round-tripping any branch created after `setup` |
| Status | Proposed |

## Problem

`pending_dest_commits` (`src/commands/sync/mod.rs:1642-1676`) takes its dest-space
boundary from `newest_dest_marker`: the newest `Setup` marker (any branch) or
`DestToSource` marker (this branch) on the *source* branch's first-parent line.
A branch cut from `main` after setup inherits only `main`'s `Setup` graft, so the
boundary is dest's tip at setup time. Every dest commit since — including the
commits gitprism itself mirrored from source `main`, which carry
`Gitprism-Branch: main` — becomes pending. Loop prevention at `mod.rs:1671` is
branch-scoped, so those mirrored commits are replayed as diffs and hard-stop on a
phantom conflict. Reproduced by execution; see the review's appendix.

## Target behaviour

> **Superseded by [decisions/0048](../../../design/decisions/0048-dest-to-source-resumes-from-the-newest-authenticated-boundary-on-either-side.md).**
> The B1/B2 predicate below (and step 4's instructions to implement it) is
> the round-1 rule decision 0048 found insufficient and replaced with a
> represented-prefix walk (case 1/case 2) after two more rejected rounds.
> Implement decision 0048's model, not this section — kept here only as the
> plan's original record of the problem and its first (wrong) target
> behaviour.

The dest→source boundary for branch `B` is the **newest commit on dest `B`'s
first-parent line** that is either:

1. **B1** — the dest commit named by the newest `Setup`/`DestToSource` marker on
   source `B`'s first-parent line (today's boundary), or
2. **B2** — a `SourceToDest` marker commit, verified against its *own* recorded
   branch (`marker::verify_self`), whose source counterpart is an ancestor of
   source `B`'s current tip (`graph_descendant_of`).

B1 remains a precondition, exactly as today: if B1 is not in the local odb or
`graph_descendant_of(dest_tip, B1)` is false, refuse with today's "isn't an
ancestor of dest's current tip" message before any walk. Only then walk dest's
first-parent line from `dest_tip` down to B1 (inclusive), newest first, and
stop at the first commit that satisfies B2; if none does, the boundary is B1.
The walk is bounded by B1 as well as by `MAX_MARKER_SCAN_COMMITS`.

The precondition is what keeps the fix fail-closed. Without it, a dest branch
force-rewound to an older mirrored commit `D` (a valid `SourceToDest` marker
whose counterpart is an ancestor of source's tip) would be accepted as the
boundary before the walk ever looked for the imported commit `C` that source
already recorded, and the divergence that today's check reports would go
unnoticed. Scenario 6 exercises exactly this.

Loop prevention inside `pending_dest_commits` stays branch-scoped. It is what
makes a customer's fast-forward of dest `release` into dest `main` still import
`release`'s content into source `main` (see scenario 3 below); switching it to
`verify_self` would silently drop that content.

Scenarios the rule must satisfy (each becomes a test in step 2):

| # | Setup | Expected boundary | Expected pending |
| --- | --- | --- | --- |
| 1 | Review repro: `release` cut from `s2`, mirrored (0044 alias `E`), customer commit `C` on dest | `E` | `[C]` |
| 2 | Customer cuts dest `hotfix` from dest `main` at `D(s2)`; developer cuts source `hotfix` at `s2`; no mirror yet | `D(s2)` (branch `main`) | `[]` |
| 3 | Customer fast-forwards dest `main` to dest `release` (`E`, `D(s3)@release`, `C`); source `main` lacks `s3` | `E` (`D(s3)`'s counterpart is not an ancestor of source `main`) | `[D(s3), C]`, both replayed |
| 4 | After a dest→source import `M(C)` on source `release` and no later mirror | `C` (B1 wins, it is the descendant) | `[]` |
| 5 | Source `release` cut at `s1`, dest `release` cut at `D(s2)`, `C0` between | `D(s1)` | `[C0, D(s2)]` |
| 6 | B1 missing from the local odb; or dest `release` force-rewound to older mirrored `D` after source imported `C` (B1 = `C`, `D` satisfies B2 but `C` is not on dest's line) | refusal with today's message, before any walk | — |

## Steps

### Step 1 — interim documentation (ship immediately, no code)

**Files.** `README.md` (dest→source section), `design/requirements/0001-workflow-and-scope.md`
open questions.

**Change.** State that until this plan lands, every branch in `config.branches`
must be present at `gitprism setup` time; a branch promoted later is refused by
`setup` and mis-synced by `sync`. Link the review finding.

**Done when.** The limitation is visible to an operator reading the README.

### Step 2 — failing tests

**Files.** `src/commands/sync/tests/dest_to_source.rs`; a new unit-test block in
`src/commands/sync/marker_scan.rs`.

**Test first.**

* Adopt the review appendix test
  `mirror_only_branch_later_added_to_config_branches_only_reflects_dest_native_commits`
  verbatim. It fails on `main` at `7c5fa19` with the phantom conflict.
* One end-to-end test per scenario 2–5 above, built from the existing fixtures
  (`bare_repo_with_a_commit_on`, `source_grafted_onto`,
  `bare_source_remote_seeded_at`, `add_independent_dest_commit_on`,
  `write_config`, `run`). Assert the set of source commits created and, for
  scenario 3, that `s3`'s content reaches source `main`.
* Unit tests for the new boundary function (step 3) covering scenario 6 and the
  `MAX_MARKER_SCAN_COMMITS` bail.

**Done when.** The new tests compile and fail for the stated reason; all 338
existing tests still pass.

### Step 3 — decision 0048

**Files.** `design/decisions/0048-dest-to-source-resumes-from-the-newest-authenticated-boundary-on-either-side.md`,
`design/decisions/index.md`, `design/log.md`.

**Change.** Context (the finding and repro), Decision (the rule in "Target
behaviour", including why loop prevention stays branch-scoped), Why (dest's
first-parent line is the only place both candidate boundaries are ordered;
`verify_self` plus the ancestor check is what makes an inherited marker safe;
Git's `graph_descendant_of` is the primitive), Consequences (one additional
bounded dest-side walk per configured branch; `setup`'s refusal of a branch with
an inherited marker is unchanged and now has `sync` as the supported path; the
0019 first-parent limitation applies to dest as it already does to source).
Record the rejected alternative: reusing the decision-0046 mapping index, which is
built *after* dest→source by 0046's own ordering.

**Done when.** The decision is in `index.md` and `log.md`, before step 4 starts.

### Step 4 — implement the boundary scan

**Files.** `src/commands/sync/marker_scan.rs`, `src/commands/sync/mod.rs`.

**Change.**

* Add `marker_scan::dest_to_source_boundary(repo, source_tip, dest_tip, branch, key) -> Result<Oid>`:
  compute B1 via `newest_dest_marker`; run today's precondition unchanged
  (`find_commit(B1)` succeeds and `graph_descendant_of(dest_tip, B1)`, or
  `B1 == dest_tip`), bailing with today's message if it fails; then walk dest
  first-parent from `dest_tip` down to B1 inclusive, newest first, bounded by
  `MAX_MARKER_SCAN_COMMITS`; return the first commit carrying a
  `verify_self(SourceToDest)` marker whose counterpart satisfies
  `repo.graph_descendant_of(source_tip, counterpart)` (treat `counterpart ==
  source_tip` as satisfied); if the walk reaches B1 without one, return B1.
  Reaching the scan limit before B1 is a limit error, as in every other scan.
* In `pending_dest_commits`, replace the `newest_dest_marker` call and the
  inline ancestor check with the new function (the function now owns that
  check). Nothing else changes; the branch-scoped loop prevention stays with an
  updated comment citing 0048.
* Update the doc comments on `scan_for_dest_marker`, `newest_dest_marker`,
  `pending_dest_commits`, and `run` that describe the old boundary.

**Done when.** Step 2's tests pass; `resolve` dest→source (which calls
`pending_dest_commits` at `resolve.rs:1193` and `:1278`) picks up the fix with no
change of its own — add one resolve test asserting it selects `C`, not a
mirrored commit, in scenario 1.

### Step 5 — audit the remaining `newest_dest_marker` caller

**Files.** `src/commands/sync/anchor.rs:62-102` (`dest_tip_accounted_for`, case 3).

**Change.** Case 3 asks whether source's newest `Gitprism-Dest-Commit` names
`dest_tip` exactly. For a promoted branch this is still correct once step 4 has
run dest→source first in the same run (the import writes a `DestToSource` marker
naming dest's tip). Write a test for the promoted-branch source→dest pass in the
same run as its first dest→source pass; if it refuses, extend case 3 to accept a
`dest_tip` that carries a `verify_self` `SourceToDest` marker whose counterpart is
an ancestor of `source_tip` (the same B2 predicate), and record that in 0048.

**Done when.** The promoted branch round-trips in both directions in one run.

### Step 6 — remove the interim documentation, record completion

**Files.** `README.md`, `design/requirements/0001-workflow-and-scope.md`,
`design/log.md`.

**Done when.** `cargo fmt --all -- --check`, `cargo clippy --workspace
--all-targets --all-features -- -D warnings`, `cargo test --workspace
--all-features`, `cargo build --release --locked` all clean; `log.md` records
the test count.

## Rejected alternatives

* **Derive the boundary from the 0046 mapping index.** The index is reconstructed
  after dest→source by design (0046, "Configured dest-to-source processing remains
  first"). Reordering would also make dest→source depend on PERF-001's fetch
  model. A direct dest-side scan needs neither.
* **Switch `pending_dest_commits` loop prevention to `verify_self`.** Loses
  content in scenario 3.
* **Teach `setup` to graft a branch with an inherited marker.** `setup` is
  one-time by decision 0006/0012; `sync` is the right owner of "what has already
  crossed".

## Verification

Release gates above, plus the six scenario tests and the resolve parity test.

## Revision history

* 2026-09-02, after plan review: the first draft stopped the dest-side walk at
  the first B1-or-B2 commit without first proving B1 is on dest's line, which
  would have accepted a force-rewound dest tip that today's check refuses. The
  B1 precondition is now explicit and the walk is bounded by B1.
