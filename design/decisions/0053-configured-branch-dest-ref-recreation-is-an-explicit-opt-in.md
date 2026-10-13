---
type: Decision
title: A configured branch whose dest ref is absent halts per branch, and `sync --recreate-dest <branch>` recreates it create-only from the nearest exact mapping
description: Fixes a v0.1.9 deadlock: a configured branch whose dest ref carried another branch's newest mirror marker (decisions/0052 Part 2 halt) or was deleted (decisions/0018 Case 1 whole-run bail) could only be recovered by temporarily removing it from `config.branches` and re-pinning the policy twice. An absent dest ref on a configured branch now halts only that branch, removed before decisions/0046 scheduling, with a diagnostic naming the new flag. `gitprism sync --recreate-dest <branch>` is the operator's explicit consent for one run: that branch skips dest→source, and source→dest restores an authenticated dest projection through the existing discovered-branch path (decisions/0046 nearest exact mapping plus a decisions/0044 marker), pushed with a create-only lease (`--force-with-lease=refs/heads/<branch>:`) so an existing or newly appearing ref is never touched. Missing anchor object, no anchor, contradictory anchor, an existing ref, or a name not configured or not local refuses. Amends decisions/0018 Case 1 and decisions/0052 Part 2's configured remedy text.
tags: [architecture, branches, recovery, cli]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-10-13T00:00:00Z }
---

# Context

Observed on v0.1.9 (`9753a4f`). After
cleaning up decisions/0052's duplicate projections, the operator pointed dest
`feat/x` (configured) at `develop`'s projection. The two dest states a
configured branch can reach without gitprism writing its marker are both
refused:

* **Tip carries another branch's newest mirror marker.** decisions/0048's
  gate refuses it. decisions/0052 Part 2 made that a per-branch halt, but its
  remedy ("restore a commit whose newest marker is its own") points back at the
  duplicate history the operator just removed.
* **Ref absent.** decisions/0018 Case 1: `sync_pair_from_dest_with_key` bails
  the whole run before source→dest's `!dest_ref_exists` arm, which would
  re-anchor via decisions/0046 and write a decisions/0044 marker, is reached.

The only exit is unconfiguring the branch for one run: two policy commits and
two `GITPRISM_POLICY_SHA256` pin updates for a bookkeeping repair, and the run
aborts every other branch meanwhile.

decisions/0018 Case 1's reasoning still holds: a configured branch's dest ref
has no routine reason to disappear, and its deletion may have discarded dest
content source never imported. Recreating it without consent would hide that
loss.

Prior art:

* Copybara doesn't infer a baseline its destination lacks; the operator
  supplies one per run: `--last-rev` ("Last revision that was migrated to the
  destination"), `--init-history` ("should be used only for the very first run
  of Copybara for a workflow"), `--force` ("Force the migration even if
  Copybara cannot find in the destination a change that is an ancestor of the
  one(s) being migrated")
  ([reference](https://github.com/google/copybara/blob/master/docs/reference.md)).
* Git has a create-if-absent compare-and-swap: `git push
  --force-with-lease=<refname>:<expect>` — "If `<expect>` is the empty string,
  then the named ref must not already exist"
  ([git-push](https://git-scm.com/docs/git-push)); locally, `git update-ref
  <ref> <new> <zero-oid>` ([git-update-ref](https://git-scm.com/docs/git-update-ref)).
  It authorizes creation only, never rewriting an existing ref.

# Decision

**1. Absent ref without the flag: per-branch halt.** In dest→source, a
configured branch with no dest ref reports `Outcome::Error` with:

> `"<branch>"` halted — round-tripped branch has no ref on dest. gitprism never
> deletes a dest ref; if the deletion was intended, run `gitprism sync
> --recreate-dest <branch>` to recreate it from the nearest exact mapping. Dest
> content not imported into source before the deletion is not restored —
> inspect and save it before deleting a configured branch's dest ref.

The branch is recorded as halted and removed from the source→dest candidate
list before mapping-distance scheduling (decisions/0046), so it is neither
recreated implicitly nor a source of secondary scheduling errors. Its
source→dest line reports `Outcome::Skipped` ("halted in dest→source"). The run
exits non-zero after every other branch is processed (decisions/0045's
pattern). Every other dest→source failure — fetch, object, conflict — keeps
its current behavior.

**2. `gitprism sync --recreate-dest <branch>` (repeatable).**

* Validated and deduplicated before any fetch or other side effect. Each name
  must pass `validate_branch_name`, be in `config.branches`, and exist as a
  local source branch; otherwise the run fails as a usage error. A discovered
  branch already recreates without consent (decisions/0017, 0018 Case 2).
* **Choosing the attempt.** The run's dest listing (or the live
  `remote_ref_exists` check when the listing can't establish absence,
  decisions/0047/0049) decides whether a recreation is attempted. It is not
  proof the ref is absent at push time; the push enforces that (below).
  * Absent: dest→source reports `Outcome::Skipped` ("dest ref absent,
    recreating on request"). Nothing to reflect.
  * Present: per-branch halt in dest→source ("`--recreate-dest` applies only
    to an absent dest ref; rerun without the flag to import and resume"),
    removed before scheduling like point 1. The flag never overrides an
    existing ref.
* **Source→dest** runs the existing `!dest_ref_exists` arm:
  `dest_anchor_for_branch` (decisions/0046 nearest exact mapping on
  `source_tip`'s first-parent line, this branch's own provenance deprioritized
  per Finding S), then `build_pending_dest_tip`, then `branch_scoped_dest_tip`
  (decisions/0044). Differences from a discovered branch:
  * `DestAnchor::None` is a per-branch halt, not decisions/0024's warning.
  * `Contradictory`, an incomplete index (decisions/0046 Finding F), and a
    mapped dest commit whose object isn't local (Finding R) halt. There is no
    fallback to an older mapping below a missing one.
  * decisions/0018 Case 2's already-merged skip stays excluded, as for every
    configured branch: the operator asked for the ref.
  * decisions/0052 reuse doesn't apply (no existing ref).
* **Accounting prefix on a conflict** (decisions/0052 Step 7's invariant).
  When replay conflicts before building any commit, the ref is still pushed
  so `resolve` has a base. Its decisions/0044 marker names the anchor's
  source commit (the resume boundary), never `source_tip`: naming
  `source_tip` would certify the conflicted commit as synced, and later
  resumes and `resolve` would skip it. Commits built cleanly before a later
  conflict keep their own source counterparts. The fix is in the shared
  `!dest_ref_exists`/rebuild fallback, so it also covers a discovered
  branch's first sync and a mirror-only rebuild that conflict before building
  anything. The no-conflict and no-op paths still name `source_tip`. No
  shared-tree replacement and no automatic resolution.
* **Create-only push.** A new `PushMode::CreateOnly` pushes with
  `--force-with-lease=refs/heads/<branch>:` (empty expected value) and is kept
  for the whole attempt. A rejection because the ref now exists — at any OID,
  including an ancestor of the proposed tip — halts the branch with "dest
  `<branch>` appeared during recreation; it was left unchanged; rerun without
  the flag to import and resume". No retry and no fallback to the normal
  resume path in a consent run. Git checks old == new before the lease, so a
  ref that already equals the proposed tip is reported up to date (porcelain
  `=`), not rejected; `CreateOnly` classifies that result as the same
  appearance halt, not acceptance. `FastForwardOnly`'s up-to-date handling is
  unchanged. Other push failures keep their current handling.

The flag is consent for one invocation. It is not policy and is not pinned.

**3. decisions/0052 Part 2 remedy text.** For a configured branch: "inspect
and save any unimported dest content, delete dest's `<branch>`, then run
`gitprism sync --recreate-dest <branch>`".

**Out of scope:** automatically accepting another branch's projection as a
configured branch's tip (an alternative proposed with the bug). It would weaken
decisions/0048's newest-marker gate, and decisions/0052 already excluded it.
`resolve` on an absent configured ref is unchanged.

# Why

* **Operator intervention, made usable.** The tool still refuses to guess
  after a configured ref disappears. The operator's consent is now a flag
  instead of two policy edits, matching Copybara's per-run baseline flags.
* **Git's own primitive for the race.** Absence is enforced by the push's
  create-only lease, not by a listing that may be stale.
* **No new lineage proof.** Recreation restores an authenticated dest
  projection/history through decisions/0046 + 0044's existing path. It is not
  a filtered snapshot of this branch's source tip and doesn't prove the
  restored tree is fully represented in it.
* **One branch, one halt.** Matches decisions/0045 and 0052 Part 2: an
  intervention on one branch must not stop unrelated ones.

# Consequences

* The observed recovery becomes: delete dest `feat/x`, run
  `gitprism sync --recreate-dest feat/x`. Dest `feat/x` is anchored on
  `develop`'s projection with a `feat/x` marker. No policy change.
* The anchor may be a dest commit reachable only from the deleted ref (e.g. a
  dest-native commit source imported); recreation republishes it and its
  history. The anchor may also carry dest-native history this branch's source
  tip doesn't represent (decisions/0052's `q` shape); the next ordinary
  dest→source pass considers it for import, subject to its existing ancestry,
  scan and conflict checks — import is not guaranteed.
* The anchor's dest commit must be present locally; otherwise the branch halts.
* A CI job never passes the flag, so a deleted configured ref stops that
  branch there until an operator acts.

# Tests (written first)

1. Configured branch, ref absent, no flag: halts in dest→source with the
   flag-naming diagnostic; excluded before scheduling (no distance or
   source→dest error for it); another configured branch and a discovered
   branch sync; run exits non-zero. Replaces decisions/0052 test 16 and
   decisions/0018's Case 1 bail test.
2. Report scenario: `main` configured and synced; `feat` configured, synced,
   dest `feat` forced to `main`'s dest tip, then deleted;
   `--recreate-dest feat` succeeds, dest `feat`'s first parent is `main`'s
   projection, its tip is a `feat`-branded marker, the next plain sync is a
   no-op, and a new source commit on `feat` syncs both directions.
3. Same, with `feat` source commits beyond the anchor: replayed onto it.
4. Anchor is an orphaned imported dest-native commit, object present:
   republished. Same with the object missing locally: halts, nothing pushed,
   no fallback to an older mapping.
5. Flag with an existing dest ref: halts, ref unchanged.
6. Ref created concurrently between listing and push, (a) at an ancestor of
   the proposed tip and (b) at an incompatible tip: the create-only lease
   rejects both, the concurrent ref is preserved, the branch halts. (c) The
   concurrent ref equals the exact proposed tip: Git reports up to date,
   `CreateOnly` halts the branch, ref preserved.
7. Flag naming a branch outside `config.branches`, an invalid name, or a
   configured branch with no local source branch: usage error before any
   fetch.
8. Flag, no exact anchor: halts. Flag, contradictory anchor: halts with the
   decisions/0046 diagnostic. Nothing pushed in either.
9. decisions/0052 Part 2 configured diagnostic names the flag.
10. CLI: `sync --recreate-dest a --recreate-dest b --recreate-dest a` parses
    and deduplicates to `{a, b}`.
11. `push_args` for `CreateOnly` emits `--force-with-lease=refs/heads/<b>:`
    before `--` and no `+` refspec.
12. The decisions/0052 `q` shape: recreation's first replayed commit
    conflicts. The pushed marker names the anchor's source commit, not the
    conflicted one; `resolve --direction source-to-dest` opens the conflicted
    commit and `--continue` pushes it on top.
13. Same with a clean commit before the conflicting one: the pushed prefix
    tip names the clean commit; `resolve` continues from it.
