---
type: Decision
title: Mirror-only force requires the local tip to match the source remote's tip
description: Fixes CODE-010 (`docs/2026-09-02_REPOSITORY_REVIEW.md`, 2026-09-07 follow-up). decisions/0039's four rewrite conditions are evaluated against this clone's local branch tip, but source's authority over a mirror-only branch lives on the source remote, not in whichever clone runs the sync — so a clone whose local branch is merely behind (the newer source object fetched but not pulled) satisfies all four and force-updates dest backwards. A fifth condition is added: `source_tip` must equal the OID the source remote advertises for `refs/heads/<branch>` (`git ls-remote` against decisions/0013's source URL, queried in the same attempt). Any mismatch, missing branch or query failure is decisions/0045's per-branch halt, never a force. Source stays authoritative for mirror-only branches; 0038, 0039, 0040 and requirements/0001 step 3 stand.
tags: [architecture, push, branches, safety]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-09-07T00:00:00Z }
---

# Context

[decisions/0039](0039-mirror-only-source-rewrites-rebuild-the-projection.md)
identifies a rewritten mirror-only branch by four conditions and, when all
hold, rebuilds dest's projection and pushes it under
[decisions/0040](0040-mirror-only-force-is-a-compare-and-swap-lease.md)'s
lease. Condition 4 is "`source_tip` no longer descends from the boundary
`dest_resume_point_for_branch` would have used", where `source_tip` is the
tip of this clone's *local* `refs/heads/<branch>`.

The 2026-09-07 review follow-up (CODE-010) reproduced this through the
compiled CLI at `f1794e5`: mirror `task` at S1; clone source at S1; advance
source to S2 and mirror it from another clone; in the stale clone run
`git fetch origin` (S2 arrives under `refs/remotes/origin/task`, local `task`
stays at S1) and then `gitprism sync`. All four conditions hold — the branch
is mirror-only, dest has the ref, dest's tip carries gitprism's marker, and
S1 does not descend from boundary S2 — so `mirror_only_rewrite_detected`
(`src/commands/sync/anchor.rs:492`) returns `true`, and the run force-updates
dest from S2's projection back to S1's, exits 0, and reports "source branch
was rewritten". Source's remote still points at S2. Nothing was rewritten.

0039's 2026-08-27 addendum (review finding F-01) already recognised the
stale-clone shape, but only for a boundary object this clone has *not*
fetched. Once the object is present, the ancestry test alone cannot tell a
behind checkout from a deliberate reset: both are "the local tip does not
descend from the boundary".

An earlier draft of this decision, written the same day, resolved the
ambiguity by halting every non-descendant case and routing all rewrites to
an operator. That draft was withdrawn before commit: it contradicted
[requirements/0001](../requirements/0001-workflow-and-scope.md) step 3 as
amended in `cdac783`, [decisions/0038](0038-branch-authority-determines-whether-history-may-be-rewritten.md)
and 0039 — all of which the project owner reaffirmed — and it made every
routine rebase or amend of a source feature branch a permanent per-branch
halt.

# Decision

**The defect is in which tip is trusted, not in whether a rewrite may be
projected.** A mirror-only branch's authority is the branch on the source
remote. This clone's local branch is evidence of that authority only when
it *is* that branch's current tip. So 0039's four conditions gain a fifth:

5. `source_tip` equals the OID the configured source remote
   ([decisions/0013](0013-repo-urls-optional-fall-back-to-env-vars.md)'s
   `source_url`) currently advertises for `refs/heads/<branch>`, read with
   `git ls-remote` during this same push attempt, after dest's tip has been
   fetched and conditions 1–4 have held.

Only when all five hold is `PushMode::ForceMirrorOnly` requested. Every
other outcome of condition 5 is [decisions/0045](0045-discovered-branch-refusals-are-per-branch-halts.md)'s
per-branch halt (`Outcome::Error`, run continues, exit nonzero):

* the advertised OID differs from `source_tip` — the local branch is behind,
  ahead of, or diverged from source; the diagnostic names the branch, the
  local tip, the source remote's tip and the prior source boundary, and
  points at [playbook 0003](../playbooks/0003-recover-from-a-stale-source-checkout-refusal.md);
* the source remote has no `refs/heads/<branch>` (`ls-remote --exit-code`
  exit 2) — the branch was deleted or renamed on source; nothing here may
  be projected on its behalf;
* the query fails (network, auth, malformed output, output over
  [decisions/0032](0032-bounded-repository-controlled-data.md)'s bound) —
  the error is reported; it is never read as "no rewrite" or "rewrite".

Condition 5 is evaluated only on the path that would force. The
fast-forward path (condition 4 false) is unchanged: a clone whose local tip
descends from the boundary still fast-forwards dest without contacting the
source remote, exactly as today. Round-tripped branches never reach any of
this and keep `FastForwardOnly` unconditionally (0038).

Within [decisions/0040](0040-mirror-only-force-is-a-compare-and-swap-lease.md)'s
retry loop, condition 5 is re-queried on every attempt alongside the dest
refetch, bounded by the existing `MAX_RACE_RETRIES`. A run therefore issues
at most one `ls-remote` against source per attempt, and only for a branch
whose first four conditions already hold — never per branch, never per run.

The check reuses the `git ls-remote --exit-code -- <url> refs/heads/<branch>`
shape `git::remote_ref_exists` already runs against dest, through
[decisions/0031](0031-centralized-git-process-runner.md)'s
standard runner with `SMALL_OUTPUT` capture, `validate_remote` and
`validate_branch_name` applied to its inputs. The advertised OID is parsed
from the first tab-separated field and must be a full object id; anything
else is the failure case above.

**Rejected alternatives**, each checked before this rule was chosen:

* *Halt every non-descendant case* (this decision's withdrawn first draft).
  Removes the capability requirements/0001 step 3 and 0038 grant, turns
  ordinary feature-branch rebases into operator work, and leaves 0040's
  lease with nothing to guard.
* *Treat the boundary's reachability from a local remote-tracking ref as
  "stale, not rewritten".* A heuristic: gitprism pushes by URL and maintains
  no remote-tracking refs of its own (0040), a tag or unrelated branch can
  keep the old tip reachable after a genuine rewrite, and a clone that never
  fetched has no such ref to consult. It also still guesses.
* *Read `source_tip` from the source remote instead of the local branch.*
  Changes what every path pushes — gitprism would sync content the operator
  has not checked out — and moves the defect rather than closing it.
* *A per-run operator flag authorising the force.* Reintroduces the
  escalation shape 0039 explicitly rejected, and cannot be pre-supplied by a
  CI trigger that does not know a rebase happened.

# Why

The stale checkout is not ambiguous once the right question is asked. 0039
asks "does the local tip descend from what dest last mirrored?" and infers
intent from the answer. Condition 5 asks the source remote "is this local
tip your tip?" — a fact, read from the authority, not an inference. If it
is, a non-descendant tip means source itself was rewritten and 0038's
authority rule applies. If it is not, this clone has no standing to speak
for source on that branch and stops.

This is the same primitive Git supplies for the dest side and 0040 already
adopted: compare-and-swap against the authority's ref. `--force-with-lease`
asks dest "are you still where I fetched you?" before overwriting; condition
5 asks source "is my checkout where you are?" before acting in source's
name. Neither guesses; both are deterministic reads of a remote ref, so
this passes AGENTS.md's bar for automation ("the safe behavior is
deterministic and established by Git").

The one residual window is the interval between the `ls-remote` and the
push: source may advance again. The projection pushed is then source's tip
*as of the check* — never older content, which is the defect CODE-010
demonstrated — and the next run fast-forwards or re-detects from the newer
tip. Dest's own concurrent movement in that window remains 0040's lease's
job; the two checks guard different sides and neither substitutes for the
other.

Cost is proportional to the event, not to the run. Sync already contacts
the source URL in dest→source (`src/commands/sync/mod.rs:1578-1660`), so
condition 5 adds no new remote, credential or capability — only one
bounded `ls-remote` when a rewrite is about to be acted on.

# Consequences

* 0039 remains the rule for recognising a mirror-only rewrite; its four
  conditions become five. 0038, 0040 and requirements/0001 step 3 are
  unchanged. 0039 carries a banner pointing here.
* A stale checkout — behind, ahead with unpushed commits, or diverged from
  source — halts that branch with a diagnostic instead of forcing dest.
  Recovery is `git merge --ff-only` from source or pushing the local commits
  to source first ([playbook 0003](../playbooks/0003-recover-from-a-stale-source-checkout-refusal.md)),
  then a rerun. No history-rewriting recovery exists or is needed.
* A branch deleted on source while its dest ref still exists halts rather
  than forcing; what happens to that dest ref stays out of scope here.
* The force path now requires read access to the source remote at push
  time. dest→source already requires it, so no deployment gains a new
  dependency; a source→dest-only deployment that never configured a
  reachable source URL will see the force path halt where it previously
  forced, with the query failure named.
* 0039's config-role hazard (removing a branch from `config.branches` makes
  it force-eligible next run) is unchanged; condition 5 narrows *when* that
  force fires, not *whether* the class is force-eligible.
* Implemented by CODE-010: `git::remote_branch_tip` queries the source
  remote's advertised tip fresh on every retry attempt inside
  `sync_pair_to_dest_with_key`'s `ForceMirrorOnly` arm; any mismatch,
  missing branch or query failure halts per decisions/0045 instead of
  forcing. 412 tests pass (408 unit + 4 `tests/cli.rs`); the CODE-010
  reproduction above now halts instead of force-updating dest.
* CODE-010's regression suite covers, through real source and dest
  remotes: the stale-fetched-but-not-pulled clone (halts, dest unchanged);
  a genuine reset and a genuine rebase with local == remote (forces under
  lease, as today); a local tip ahead of source (halts); a branch deleted on
  source (halts); an unreachable source remote (halts, error named); and
  the stale-checkout recovery — `merge --ff-only`, rerun, no-op rerun.
