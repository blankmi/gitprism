---
type: Decision
title: A rewritten mirror-only source branch rebuilds its dest projection instead of being refused
description: A rewritten mirror-only branch is a positively identified state — mirror-only, dest ref exists, a prior gitprism marker exists, source no longer descends from the previous boundary — not a failed-race fallback. On that state, dest_resume_point_for_branch's divergence refusal is bypassed and the projection is rebuilt from the graft-derived (boundary, dest_tip) newest_dest_marker_opt_for_branch already reads off source's own history, then force-pushed via decisions/0038's ForceMirrorOnly. Round-tripped branches and dest→source are unaffected. Extends decisions/0038 and narrows its resolve.rs call-site guidance.
tags: [architecture, push, concurrency, branches]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-21T00:00:00Z }
---

# Context

[decisions/0038](0038-branch-authority-determines-whether-history-may-be-rewritten.md)
permits force-updating a mirror-only dest branch to match a deliberately
rewritten source branch. Starting its implementation surfaced that the
existing code refuses such a branch before any push is attempted, so 0038's
force path is currently unreachable. Traced against `src/commands/sync.rs`
for a rewritten mirror-only branch that already has a dest ref:

* `dest_ref_exists` is true, so `dest_tip` is the old, pre-rewrite mirrored
  tip, fetched at `sync.rs:316-340`.
* `dest_resume_point_for_branch` (`sync.rs:1003`) is called for `boundary`
  (`sync.rs:354-365`).
* `dest_tip_is_accounted_for` (`sync.rs:939`) **passes**, via its Case 1: the
  old dest tip still carries gitprism's own `SourceToDest` marker from the
  earlier sync.
* `newest_source_marker` (`sync.rs:1867`) then returns the **pre-rewrite**
  source tip as `boundary` — the newest `Gitprism-Source-Commit` trailer
  reachable from the old dest tip.
* `boundary != source_tip`, so `dest_resume_point_for_branch` runs
  `repo.graph_descendant_of(source_tip, boundary)` (`sync.rs:1040`) — false,
  because a rewrite (rebase, amend, reset) gives the branch a new commit off
  the same parent rather than a descendant of the old one.
* `dest_resume_point_for_branch` returns `Ok(None)`, and the caller's
  `.with_context(...)?` turns it into "dest branch \"X\" isn't at a point
  this clone can safely build on."

This fires for every history-editing rewrite of a mirror-only branch —
rebase, amend, or branch reset — and it fires before `sync_pair_to_dest`
ever reaches a push, so 0038's `ForceMirrorOnly` variant has no call site
that can be exercised for the case it exists for.

# Why the check is wrong for this branch class

`dest_resume_point_for_branch`'s refusal protects against a real hazard:
dest may hold content source does not yet have (independent commits,
dest→source not yet run this session), and building a fresh chain onto a
tip gitprism doesn't recognize could silently drop that content. For a
**round-tripped** branch that hazard is real — dest→source can land
independent dest content on that branch (decisions/0017) — and the refusal
must stand.

For a **mirror-only** branch the premise is false. [decisions/0017](0017-source-to-dest-mirrors-every-branch.md)
established that dest→source only ever consumes branches named in
`config.branches`; a mirror-only branch's dest ref never receives content
gitprism doesn't already have on source. Source is unconditionally
authoritative for that branch, and dest's copy is a projection with nothing
independent to protect. The divergence `dest_resume_point_for_branch`
detects here isn't dest holding something source lacks — it's source having
moved out from under a description of source's own past state.

# The decision, in the project owner's own words

> Resume-point divergence protection applies to round-tripped branches. A
> rewritten mirror-only source branch may rebuild its destination projection
> from the shared graft/base and replace the old mirror history.

```
round-tripped branch:
    resume point valid?
        yes -> normal fast-forward sync
        no  -> stop, operator reconciles
mirror-only branch:
    resume point valid?
        yes -> normal fast-forward sync
        no  -> rebuild authoritative projection
               -> force-update the mirror-only dest branch
               -> if dest moves during the operation, refetch/recompute
```

# Decision

**A rewritten mirror-only branch is identified by four conditions checked
together, not by exhausting retries against a failure.** All four must
hold:

1. the branch is mirror-only — absent from `config.branches`;
2. `dest_ref_exists` is true — dest already has a same-named ref;
3. a previous gitprism marker is found on that dest ref (`dest_tip_is_accounted_for`'s
   Case 1 or Case 3) — a prior sync genuinely happened;
4. `source_tip` no longer descends from `dest_resume_point_for_branch`'s
   would-be boundary — the exact `graph_descendant_of` check already in the
   code, now inspected on failure instead of only on success.

When all four hold, gitprism has positively identified a source-side
rewrite, not merely failed to find a resume point. It rebuilds the
projection instead of refusing.

**Force is not an escalation after failed retries.** This is the load-
bearing correction of an earlier draft of this idea, and must not be
blurred. "After retries are exhausted, force" reads as a general escalation
policy — any persistent non-fast-forward eventually gets forced — and a
future contributor could plausibly generalize that framing to a
round-tripped branch's persistent divergence, exactly the operator boundary
[decisions/0038](0038-branch-authority-determines-whether-history-may-be-rewritten.md)
draws and [AGENTS.md](../../AGENTS.md)'s operator-intervention rule protects.
The four conditions above are checked directly, not inferred from retry
exhaustion, and they say nothing about how many times a push was attempted.
[decisions/0009](0009-push-race-refetch-and-recompute.md)'s
refetch-and-recompute keeps its existing, narrower meaning in both push
modes: resolving a genuine concurrent race — another writer moved dest's
tip while this run was building on it — nothing about that mechanism
changes here. A rewrite and a race are different failures that happen to
surface at the same call site, and this decision only changes how the
rewrite is recognized.

**What is rebuilt from.** The rebuild base is not new base-finding logic.
It is the same path `sync_pair_to_dest` already takes when `!dest_ref_exists`
(`sync.rs:387-388`): `newest_dest_marker_opt_for_branch(repo, source_tip,
branch, state_key)`, which scans source's own first-parent history for the
nearest `Gitprism-Dest-Commit` trailer and returns `(boundary, dest_tip)`
straight from that trailer — the graft-derived shared ancestry
[decisions/0006](0006-setup-uses-real-shared-history.md) established, read
off source's side rather than fetched from a dest ref. A detected rewrite
takes this same `(boundary, dest_tip)` in place of the one
`dest_resume_point_for_branch` refused to produce, then proceeds through
`pending_commits`/`build_pending_dest_tip` exactly as the `!dest_ref_exists`
path already does — no second rebuild mechanism.

**Who may force, narrowly.** Only `sync_pair_to_dest` may request
`PushMode::ForceMirrorOnly`, and only once it has established both that the
branch is absent from `config.branches` and that the four-condition rewrite
above was detected. `sync_pair_from_dest` (dest→source) and the
round-tripped source→dest path stay `FastForwardOnly` unconditionally, per
0038.

This narrows 0038's guidance for `resolve.rs`. 0038 classified
`resolve.rs:335`/`resolve.rs:579` (both inside `resolve_source_to_dest`) by
`config.branches` membership, since `Direction::SourceToDest` accepts any
local branch and so could reach a mirror-only one. Under this decision's
narrower rule, `ForceMirrorOnly` requires a *positively detected rewrite* —
a resume-point refusal that `resolve`'s own flow doesn't reproduce, since
`resolve` operates on an already-in-progress conflict resolution, not a
fresh resume-point computation from a discovered dest ref state. `resolve`
never performs this detection, so every `resolve` push — `resolve.rs:335`,
`:579`, and `:1324` — stays `FastForwardOnly`, regardless of branch
authority. This refines, not contradicts, 0038: 0038's own table still
holds (mirror-only source→dest *may* force); this decision says the one
place that force is actually requested from is `sync_pair_to_dest`'s
rewrite-detection point, and `resolve.rs` is not that place. 0038 itself is
not edited.

**The authority invariant.** Independent dest advancement on a mirror-only
branch may be discarded *only* because the branch is declared mirror-only by
its current absence from `config.branches`. This is the exact assumption
that licenses the force update: it is true precisely because
[decisions/0017](0017-source-to-dest-mirrors-every-branch.md) guarantees
dest→source never touches a branch outside that list, so nothing dest holds
on a mirror-only branch is dest's own independent contribution gitprism is
obligated to preserve. If a branch is in `config.branches`, this whole
mechanism does not apply to it — the four-condition check's first clause
exists precisely to keep the two branch classes from being conflated at the
one call site (`sync.rs:354-365`) where they currently share code.

**Config-role hazard, restated by reference.** [decisions/0038](0038-branch-authority-determines-whether-history-may-be-rewritten.md)
already records that branch authority is decided by current
`config.branches` membership, re-evaluated every run, with no persisted
"this branch is round-tripped" record — so removing a branch from that list
makes its dest history force-eligible on its very next sync. This decision
does not change that hazard or add mitigation for it; it only adds a second
trigger (a detected rewrite) for the force path that hazard already
describes.

# Why

* **The four-condition check matches what the code already computes**,
  rather than adding a new signal. Conditions 1–2 are existing branches in
  `sync_pair_to_dest`; condition 3 is `dest_tip_is_accounted_for`, already
  called; condition 4 is `dest_resume_point_for_branch`'s own final
  `graph_descendant_of` check, already computed and simply consulted on its
  `false` result instead of discarded. No new git operation is added to
  detect a rewrite.
* **Rejecting the escalation framing is the point, not an aside.** An
  "escalate to force after N failures" policy would be a second, implicit
  authority test running alongside 0038's explicit `config.branches` test —
  exactly the kind of generalizable-by-a-future-contributor automation
  `AGENTS.md`'s "prefer operator intervention over novel automation" rule
  warns against. Naming the four conditions as a positive identification
  keeps the authority test singular: mirror-only-ness from `config.branches`,
  checked once, in one place.
* **Reusing `newest_dest_marker_opt_for_branch` rather than inventing a
  second rebuild path** follows the same "one mechanism per property"
  reasoning [decisions/0016](0016-both-directions-merge-via-real-git-merge-tree.md)
  and [decisions/0018](0018-branch-deletion-failure-modes.md) already apply
  elsewhere in this file: the `!dest_ref_exists` path and the
  rewrite-detected path both answer "what does source's own graft-derived
  ancestry say the dest-space boundary is," and a rewrite is exactly the
  case where dest's own ref can no longer be trusted to answer that
  question — the same condition `!dest_ref_exists` was already built for.
* **`resolve.rs` staying fast-forward-only is a consequence of what
  `resolve` actually computes, not an added restriction.** `resolve`
  resumes a specific, already-open conflict; it never runs
  `dest_resume_point_for_branch`'s ancestry check on a fresh dest fetch the
  way `sync_pair_to_dest` does, so it has no occasion to positively identify
  a rewrite in the first place. Narrowing 0038's guidance here doesn't
  remove a capability `resolve` needed — it removes a classification 0038
  suggested but that this decision's stricter trigger condition never
  actually reaches from `resolve.rs`.

# Consequences

* **What's discarded on a rebuild, and why it's safe.** The old mirrored
  dest history for that branch — everything since the prior graft/marker
  boundary — is replaced wholesale by a freshly filtered chain built from
  source's current, rewritten history. Safe because nothing on a
  mirror-only branch's dest ref is content dest independently owns (the
  authority invariant above); the old mirror was only ever a rendering of a
  source state that no longer exists.
* **decisions/0009 is unchanged in mechanism.** Refetch-and-recompute keeps
  handling genuine concurrent races — a benign advance of dest's tip by
  another writer during this run — identically in both `PushMode` values.
  This decision does not touch retry counting, porcelain-based rejection
  classification, or when a retry fires; it only changes what happens when
  the *initial* resume-point computation, not a push rejection, indicates a
  rewrite.
* **decisions/0018's deletion heuristic is untouched.** `already_merged_into_a_landing_branch`
  and its no-persisted-state, content-based classification are unrelated to
  this call path — they only run once `sync_pair_to_dest` is past the
  divergence check, and only in the `!dest_ref_exists` branch this decision
  does not modify.
* **Config-role hazard is unchanged**, per 0038: reviewing a `config.branches`
  diff remains reviewing which branches gitprism may rewrite outright, now
  including under this second trigger.
* **Tests the implementation commit must add:**
  * the full four-row branch-role × direction matrix from 0038 (round-tripped
    source→dest, round-tripped dest→source, mirror-only source→dest,
    mirror-only dest→source — the last is N/A, never synced), re-verified
    against this decision's changed call path;
  * a mirror-only branch rewritten via `git rebase`: the old dest history is
    replaced, dest ends at the rewritten tip;
  * a mirror-only branch rewritten via `git commit --amend`: same outcome;
  * a mirror-only branch rewritten via a hard branch reset to an earlier
    commit plus a new commit: same outcome;
  * a test for the authority invariant: a mirror-only branch with genuine
    independent dest advancement (not a rewrite — dest simply has an extra
    commit gitprism never built) is discarded by the rebuild, and the test
    must show this is licensed specifically by `config.branches` absence —
    e.g. by running the identical dest-side state against a branch that *is*
    in `config.branches` and confirming that one stops instead;
  * a benign race on a mirror-only branch (another clone legitimately
    advances dest mid-run, no source rewrite) is still incorporated via
    refetch-and-recompute, not force-clobbered — force must not fire;
  * a round-tripped branch with genuine divergence still stops with 0038's
    operator-facing message and never forces, even when the same four-
    condition shape coincidentally holds except for `config.branches`
    membership;
  * role-aware guard assertions, extended from 0038's: round-tripped pushes
    (both `sync.rs` call sites, both round-tripped `resolve.rs` call sites)
    never emit `--force`, `--force-with-lease`, or a `+`-prefixed refspec;
    additionally, **every** `resolve.rs` push — including a mirror-only
    branch reached via `resolve_source_to_dest` — never emits them either,
    confirming this decision's narrowing of 0038's `resolve.rs` guidance.

# Prior art

[decisions/0038](0038-branch-authority-determines-whether-history-may-be-rewritten.md)
already found GitLab push mirroring force-updates a diverged mirror by
default, with an opt-in "Keep divergent refs" setting to suppress it —
that finding is not re-researched here; it establishes precedent for
force-*updating* a ref to match an authoritative upstream, which this
decision still relies on for the push step itself.

No source in `design/references/` (josh, Copybara, git-subtree,
git-filter-repo, jujutsu) was found to speak to *rebuilding* a projection
from a shared base after a detected upstream rewrite, as distinct from
simply force-updating a ref to the upstream's current tip. GitLab's own
push-mirror documentation describes the force-update outcome, not how (or
whether) it rebuilds anything on its own side before pushing — a mirror
push there is a plain ref update, with no filtering step in between to
rebuild. gitprism's rebuild step exists only because gitprism, unlike a
plain mirror, must re-filter source's rewritten history before it can be
pushed at all; nothing checked treats that as a distinct design question
worth prior art of its own.

# Addendum: no code changed in this commit

This decision was written before any implementation of decisions/0038
landed. Attempting that implementation is what surfaced the unreachable
force path described in Context — the investigation stopped 0038's
implementation and produced this decision instead of a workaround. 0038's
`PushMode` enum, its call-site classification, and this decision's rewrite
detection and rebuild path are all still to be implemented together.

# Addendum: the rebuild base is pushed even when no commit is constructed

An external review found that `sync_pair_to_dest`'s original implementation
computed `new_dest_tip` as `build.new_tip.or((!dest_ref_exists).then_some(dest_tip))`
unconditionally, including in the detected-rewrite arm. In that arm
`dest_ref_exists` is always `true`, so the expression reduces to
`build.new_tip.or(None)`: whenever `build_pending_dest_tip` didn't actually
construct a commit, nothing was pushed at all, and dest silently kept the
discarded pre-rewrite history while the run still reported success.

This is not a rare edge case. It is the shape of the single commonest
rewrite there is — `git reset --hard` to an earlier commit, then a
force-push with no new commit of its own. When the branch is reset straight
back to a commit that already carries a `Gitprism-Dest-Commit` trailer (the
shared graft, most often), the rebuild's own boundary
(`newest_dest_marker_opt_for_branch`) equals source's new tip exactly, so
`pending_commits(boundary, source_tip)` is empty and `build_pending_dest_tip`
never enters its loop. A second, distinct way to reach the same `new_tip:
None` result: `pending` is non-empty, but every pending commit's merge, once
filtered, nets to no tree change against the rebuild base (the loop's own
`if merged == parent_commit.tree_id() { continue; }`, kept from
requirements/0001's "must not push an empty commit") — e.g. a rewrite whose
replacement commits touch only excluded paths.

The fix: when the push mode is `ForceMirrorOnly` (i.e. this arm's detected
rewrite, not the ordinary `!dest_ref_exists` no-op case this decision's
`(!dest_ref_exists).then_some(dest_tip)` fallback already handled), the same
fallback to `dest_tip` — which in this arm is the graft-derived
`rebuild_dest_tip`, not dest's stale fetched tip — applies regardless of
whether `build_pending_dest_tip` constructed anything. A detected rewrite
with nothing to build is still a rewrite: dest's projection must be rewound
to the shared base source's current history now supports, not left pointing
at history source itself has disowned. `FastForwardOnly`'s own fallback is
untouched — it only ever applies to a genuinely brand-new branch, per this
decision's original text, and stays exactly `(!dest_ref_exists).then_some(dest_tip)`.

The reporter's completion line for this specific case (dest's ref moved, but
to the rebuild base, not to a newly built commit) says so plainly — "rebuilt
from the shared graft; no new commits were needed" — rather than the
ordinary done/skip wording, so a rewind is never misread as new commits
having been pushed.

No condition, boundary computation, or rewrite-detection logic changes;
this addendum only corrects what happens with the boundary and rebuild base
this decision already established once `build_pending_dest_tip` reports
nothing to build from them.

# Addendum: a missing boundary object is confirmed as a rewrite on a
non-shallow clone

A real deployment hit condition 4's `repo.find_commit(boundary).is_err()`
branch (`sync.rs:1278`, mirrored in `dest_resume_point_for_branch`'s own
`sync.rs:1220`) on an ordinary `git commit --amend && git push --force` of a
mirror-only branch — exactly the case this decision names as in scope
("a mirror-only branch rewritten via `git commit --amend`: ... the old dest
history is replaced"). The push was refused with the generic resume-point
message instead of rebuilding.

**Why.** The boundary trailer names the pre-rewrite tip. Once force-pushed,
that commit is unreachable from any ref on source; a CI job that clones or
fetches fresh (GitLab Runner's default) never retrieves it, so
`find_commit(boundary)` fails before the ancestry check that would otherwise
identify the rewrite ever runs. The code already treats a missing boundary
object conservatively — "a different failure than a rewrite" (the doc
comment above `mirror_only_rewrite_detected`) — which was the right call
when the object could be missing for either of two reasons: the branch was
genuinely rewritten, or this clone simply hasn't fetched enough history yet
to have it (still an ordinary fast-forward underneath). Those two cases were
indistinguishable from inside `mirror_only_rewrite_detected` as written, so
it refused both alike.

They stop being indistinguishable once it's known whether this clone's
history for the branch is complete. `Repository::is_shallow` (git2, backed by
`git_repository_is_shallow`) answers exactly that, for the whole repository,
using no new git operation gitprism doesn't already have a handle for. On a
**non-shallow** clone, a missing boundary object has no remaining
explanation other than the commit no longer being reachable from source's
current history — the same conclusion condition 4's ordinary
`graph_descendant_of` check reaches when the object *is* present and simply
isn't an ancestor. On a **shallow** clone, the ambiguity is real and
unresolved by anything gitprism can check locally; today's refusal, with its
existing "fetch/pull the latest source history first" guidance, stands
unchanged.

**Decision.** In `mirror_only_rewrite_detected` only — not
`dest_resume_point_for_branch`, whose conservative `None` for a round-tripped
branch must never be reinterpreted as a confirmed rewrite — replace

```
if boundary == source_tip || repo.find_commit(boundary).is_err() {
    return Ok(false);
}
```

with a missing-object branch that checks `repo.is_shallow()`: shallow keeps
returning `Ok(false)` (unchanged); non-shallow returns `Ok(true)` directly,
since there is nothing further to check — no boundary commit exists to run
`graph_descendant_of` against, and its absence *is* the positive
identification. `boundary == source_tip` keeps its own unconditional `Ok(false)`,
shallow or not: that case means source hasn't moved past the recorded
boundary at all, never a rewrite regardless of clone completeness.

This does not loosen condition 3 (a prior gitprism marker must still be found
before this function is even reached) or narrow who may request
`ForceMirrorOnly` (still only `sync_pair_to_dest`, still only after this
check). It only replaces one conservative guess ("missing means unknown, so
refuse") with a deterministic read of a fact the object database already
knows, matching this decision's original "no new git operation is added to
detect a rewrite" reasoning.

**Consequences.**
* A shallow CI checkout still refuses on a rewrite whose boundary object
  went missing, unchanged from today — this addendum does not eliminate the
  ambiguity there, it eliminates it only where the object database has
  enough information to actually resolve it. Deployment guidance (fetch depth
  for branches gitprism runs against) is deployment guidance, tracked
  separately in `design/playbooks/`, not folded into this code change.
* Every rewrite shape this decision already lists (amend, rebase, hard reset)
  can now hit this path, not just `git reset --hard` to an already-marked
  commit (the prior addendum's case) — any of them discards the boundary
  commit outright when it was the exact tip dest last synced from, and a
  fresh clone never has it.
* **Test gap found alongside this addendum, must be closed together**: the
  existing amend/rebase/reset tests (`sync_pair_to_dest_rebuilds_a_mirror_only_branch_rewritten_by_*`)
  rewrite `source_repo` in place via `branch(..., force: true)` — the
  pre-rewrite commit stays present in the same object database throughout,
  so `find_commit(boundary)` trivially succeeds and none of them exercise
  this addendum's path at all. At least one test must instead model a real
  deployment: rewrite source in one repo, then run `sync_pair_to_dest`
  against a **separate, freshly fetched clone** (mirroring how `dest_dir`
  is already its own bare repo in these fixtures) that never fetched the
  pre-rewrite tip, on both a shallow and a non-shallow clone of it — shallow
  keeps refusing, non-shallow rebuilds.

# Addendum: narrow the missing-object branch to `NotFound`, and two
documented limitations

A review of the previous addendum's implementation found
`repo.find_commit(boundary).is_err()` (`sync.rs:1281` at the time) treats
*any* libgit2 error as "object missing," not just a genuinely absent object.
On a non-shallow clone that misread now resolves to `Ok(true)` — a confirmed
rewrite, force-pushed via `PushMode::ForceMirrorOnly` — for a transient ODB
error or corruption gitprism should instead fail loudly on, per
[AGENTS.md](../../AGENTS.md)'s "if resolution requires guessing intent...
fail clearly and let the operator resolve it."

**Decision.** `mirror_only_rewrite_detected` now matches on
`repo.find_commit(boundary)` directly: `Err(error) if error.code() ==
git2::ErrorCode::NotFound` is the only branch read as "missing," taking the
same `!repo.is_shallow()` path this addendum already established; every
other `Err` propagates via `.with_context(...)` as a real error instead of
being guessed either way. This is the same `ErrorCode::NotFound` guard
already used at `sync.rs:1618` for an unrelated missing-object case,
narrowed here rather than introducing a new pattern.

**Empirically, this rarely matters in practice but is still worth the
narrowing.** `git2::Repository::find_commit` on an object that exists but
isn't a commit (a tree or blob oid, tested directly) also reports
`ErrorCode::NotFound` — libgit2 folds "wrong type" into the same code as
"absent," just under `ErrorClass::Invalid` instead of `ErrorClass::Odb`, so
that particular case is not actually distinguishable from a genuinely
missing object this way and stays on the missing-object path either way. A
non-`NotFound` error is realistic, though: a loose object file this process
can't read (permission denied, corruption) reports `ErrorCode::Locked`
(`ErrorClass::Os`), confirmed with a chmod'd loose object in a test — the
new `mirror_only_rewrite_detected_propagates_a_real_lookup_failure_instead_of_guessing`
test, which chmods a real boundary commit's own loose object file to `0o000`
and asserts `Err`, confirmed to fail (as `Ok(true)`) against the
pre-narrowing code.

**Two known, accepted limitations, documented rather than fixed** — per
AGENTS.md's default ("the default decision is to stop and involve the
operator, unless the project owner explicitly decides the added automation
is necessary"), neither is being solved here, both fail safe (refuse rather
than wrongly force-push) or are already outside gitprism's control:

* **`is_shallow()` is a whole-repository flag, not scoped to the branch
  being evaluated.** One shared `Repository` handle serves every branch in a
  `sync` run (`sync.rs`'s `run`); if any ref in that checkout was ever
  shallow-fetched, `is_shallow()` reads `true` for every branch's check that
  run, even a fully-fetched branch with a genuine rewrite — that branch
  falls back to refusal instead of rebuilding. Git and libgit2 don't expose
  finer granularity than this: `git_repository_is_shallow` is backed only by
  whether `.git/shallow` exists and is non-empty (confirmed against the
  git2-rs source), and `.git/shallow`'s own content is a flat list of
  grafted shallow-boundary commit ids from the *current* shallow history, not
  a per-branch or per-object record of what's missing — there is nothing to
  scope the check to a single branch with. This fails safe: the outcome is
  an unnecessary refusal, never a wrong force-push.
* **A non-shallow read does not strictly prove the odb holds everything
  reachable from `source_tip`.** `is_shallow()==false` is treated as proof
  the object database holds everything reachable from `source_tip` — true
  for every clone gitprism itself creates, but not strictly true for a clone
  made with `git clone --reference`/`--shared`, or one relying on alternates
  (`.git/objects/info/alternates`): such a clone is never marked shallow, yet
  can lose objects if the alternate object store it depends on is
  independently pruned or garbage-collected later. gitprism doesn't create
  such clones itself and has no supported workflow that does, but doesn't
  preclude an operator from pointing it at one.
