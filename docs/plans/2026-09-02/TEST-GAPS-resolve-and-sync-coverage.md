# Plan TEST-GAPS — close the resolve and sync coverage gaps

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 8 (testing gaps) and action plan row "P2 — Add the missing resolve state-transition tests" |
| Severity / priority | — / P2 |
| Effort | Medium |
| Decision required | No |
| Depends on | CODE-001 and TEST-001 completed; only worktree-location-specific cases coordinate with proposed OPS-001 |
| Status | Partially implemented as of 2026-09-07: promoted-branch resolve regression and binary smoke tests landed through CODE-001 and TEST-001; remaining work below |

## Status checked 2026-09-07

Completed elsewhere:

* Step C's promoted-branch resolve test is
  `resolve_dest_to_source_selects_the_customer_commit_not_a_mirrored_commit_after_a_branch_is_promoted`
  in `src/commands/resolve.rs`.
* Step D's built-binary coverage is in `tests/cli.rs` (three smoke tests).

The test-module split, remaining resolve state-transition tests, and remaining
step D coverage are still proposed. CODE-003's committer validation tests remain
with that plan. CODE-001 and TEST-001 dependencies are satisfied; OPS-001 is
still proposed.

New stale-source and generated-message regressions belong to CODE-010 and
CODE-011. They are release-priority work and must not wait for this plan.

Each bullet is one test. Names follow the repository's sentence-style
convention. Fixtures come from `src/testutil.rs` and the `sync/tests` helpers;
where a test needs a new helper, it is named.

## Step A — split resolve's tests out of `resolve.rs`

At `f1794e5`, `src/commands/resolve.rs` is 3,446 lines, with tests starting
at line 1,469. Move the existing
`mod tests` into `src/commands/resolve/tests/{mod.rs, source_to_dest.rs,
dest_to_source.rs, state.rs}` following the 2026-08-28 `sync` split. Mechanical;
no test changes. This split is optional preparation, not a prerequisite for
correctness regressions or steps B–D.

## Step B — resolve, source→dest

* `start_refuses_when_a_source_to_dest_resolution_is_already_in_progress` —
  start twice; second run names the existing state ref and the worktree path.
* `continue_refuses_while_the_worktree_still_has_conflict_markers` — start, do
  not edit, `--continue`; error lists the conflicted paths; state ref unchanged.
* `continue_refuses_when_the_source_branch_moved_after_start` — start, add a
  commit to source `feature`, `--continue`; error says the branch moved; nothing
  pushed.
* `continue_refuses_a_state_ref_whose_mac_does_not_verify` — flip one byte of
  the state commit's MAC trailer (`Repository::commit` a rewritten message on
  the state ref); `--continue` refuses before touching the worktree.
* `start_refuses_a_worktree_path_registered_to_another_repository` — register
  the reserved path as a worktree of a second repo; start reports it, removes
  nothing.
* `start_refuses_a_mirror_only_branch_with_no_dest_ref` — decision 0039's
  refusal text; no state ref created.
* `start_reports_a_lost_race_when_pushing_the_clean_prefix` — advance dest
  `feature` between fetch and push (helper: a `git::push` hook is not available;
  instead pre-advance dest after the fixture's fetch by calling the internal
  function with a stale `dest_tip`); assert `RejectedRefMoved` surfaces as the
  documented message and no state ref exists.
* `continue_picks_a_merge_commit_with_mainline_one` — the conflicting source
  commit is a two-parent merge; after resolution the built dest commit's tree
  equals the merge's first-parent diff applied. This direction cherry-picks a
  synthetic single-parent filtered patch, so do not expect `-m 1` on its argv.
  Test the real dest→source merge pick with mainline one separately in step C.

## Step C — resolve, dest→source

* `start_refuses_when_a_cherry_pick_is_already_in_progress` — create
  `.git/CHERRY_PICK_HEAD`; message at `resolve.rs:1170` appears.
* `continue_refuses_when_source_remote_rejects_the_finish_push` — seed the
  source remote ahead of the local branch before `--continue`; assert the remote
  remains unchanged while the authenticated resolution is committed locally,
  and the message hands reconciliation to the operator. `finish` deliberately
  updates the local branch before pushing; expecting no local advance would
  contradict current behavior.
* `dest_to_source_resolve_picks_a_merge_with_mainline_one` — resolve a real
  two-parent dest commit and verify the resulting first-parent change and marker.
* `dest_to_source_resolve_selects_the_customer_commit_on_a_promoted_branch` —
  CODE-001 scenario 1 through `resolve`; lives in CODE-001's plan, listed here
  for completeness.

## Step D — sync and setup

* `setup_fetch_failure_on_a_later_branch_creates_nothing` — three configured
  branches; the third's dest ref is deleted before `run` (delete it in the bare
  repo after `write_config`). Setup has no remote listing and fetches every
  branch before the commit phase (`setup.rs:181-201`), so this fails in
  preflight with nothing to roll back. Assert no local branch was created,
  no control file written, and HEAD unchanged. This is a preflight-atomicity
  test, not a rollback test; rollback is already covered by the commit-phase
  injection tests at `setup.rs:1710` and `:2290`, which CODE-005 step 3
  extends.
* `dest_side_mapping_entry_truncation_taints_the_index` — `reconstruct_with_scan_limit`
  with a dest head whose first-parent line exceeds the limit; assert
  `is_truncated()` and that a lookup for a source commit *inside* the scanned
  part still refuses (Addendum 1 rule).
* `exclude_branch_ignores_its_own_alias_but_keeps_a_siblings` — decision 0044
  empty alias `E@task` on top of `D@feature`; anchoring `task`'s rebuild resolves
  to `D`, not `E`; anchoring a third branch resolves to `D` as well.
* Committer identity with control characters — in CODE-003's plan.
* Any test that runs the built binary — in TEST-001 step 5.

## Revision 2026-09-07

Corrected the rejected-push assertion and distinguished the synthetic
source→dest patch from dest→source's real merge cherry-pick. Updated module
size and narrowed dependencies so useful coverage is not blocked on cleanup.

## Verification

`cargo test resolve` and `cargo test sync`; each new test is checked to fail
when its guarding condition is commented out (one-line sabotage, reverted),
recorded in the commit message as "verified to fail against …" in the style
`design/log.md` already uses.
