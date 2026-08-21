---
type: Decision
title: Branch authority, not push direction, determines whether history may be rewritten
description: Round-tripped branches (config.branches) are shared history, fast-forward-only in both directions; persistent divergence is an operator boundary. Mirror-only branches are a one-way projection of source and may be force-updated to match a deliberately rewritten source branch (trigger condition superseded by decisions/0039). No lease mechanism (--force-with-lease) is adopted for round-tripped branches; the blanket "no lease for any branch type" rule is superseded by decisions/0040, which adopts an explicit lease as compare-and-swap concurrency control around an already-authorized mirror-only force. Formalizes requirements/0001 step 3 as amended in cdac783.
tags: [architecture, push, concurrency, branches]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-21T00:00:00Z }
---

> **Superseded in part by [decisions/0039](0039-mirror-only-source-rewrites-rebuild-the-projection.md).**
> This decision's retry-escalation framing for mirror-only force — "force
> only once decisions/0009's bounded refetch-and-recompute is exhausted" —
> is overturned. 0039 requires a rewritten mirror-only branch to be
> **positively detected** by four direct conditions, checked before any push
> is attempted; retry count never licenses force, and 0039 explicitly warns
> against reading "force after exhausted retries" as a general escalation
> policy. Passages below that still frame force that way are marked inline.
>
> **What this decision still decides, unedited by 0039:** branch authority
> itself. Round-tripped branches (`config.branches`) remain shared history,
> fast-forward-only in both directions, with persistent divergence handed to
> the operator rather than resolved automatically. Mirror-only branches
> remain a one-way projection of source that may be force-updated to reflect
> a deliberate source-side rewrite. That table and its reasoning stand;
> only the trigger for reaching the force path changed.

> **Also superseded in part by [decisions/0040](0040-mirror-only-force-is-a-compare-and-swap-lease.md).**
> This decision's blanket "no lease mechanism (`--force-with-lease`) is
> adopted for any branch type" conclusion is revised: an already-authorized
> mirror-only force now uses an explicit
> `--force-with-lease=refs/heads/<branch>:<fetched-dest-oid>` as
> compare-and-swap concurrency control against the dest tip gitprism itself
> fetched, closing a lost-update race an unconditional `+`-prefixed refspec
> could not detect. **What still stands, unedited by 0040:** a lease must
> never be what enables force on a round-tripped branch — git's own
> fast-forward-only default remains their sole protection, exactly as this
> decision's authority table requires. Passages below that state the old
> blanket rule are marked inline.

# Context

`requirements/0001` step 3 originally read "This push to dest must be
**fast-forward only** — never a force-push" with no qualification. Commit
`cdac783` amended it, because it was written before
[decisions/0017](0017-source-to-dest-mirrors-every-branch.md) introduced the
round-tripped/mirror-only distinction and so could not have accounted for
it. The amended text: a round-tripped branch (step 4's branches, whose
dest-side changes flow back to source) is shared history and must stay
fast-forward-only, operator-reconciled on divergence; a branch that only
ever flows source→dest is a projection of source's history, never imported
back, and "may be force-updated when source's own branch was deliberately
rewritten." The **Explicit constraints** section is unchanged and still
rules out a `git-filter-repo`-style workflow — "rewriting history and
force-pushing dest on every sync" — because "dest is treated as shared
history other clones depend on." This decision is the architecture record
for the amended step 3, and must reconcile it with that unchanged
constraint rather than read as silently contradicting it (see Why).

**A separate, rejected proposal.** An external review proposed
`--force-with-lease` as concurrency hardening for the push race
[decisions/0009](0009-push-race-refetch-and-recompute.md) already handles.
Verified against real git: a lease using the correct expected old-value
still force-pushed a divergent history, and the remote reported `(forced
update)` — a lease is a force push gated on a compare-and-swap, not a
substitute for a fast-forward check. Adopting it for round-tripped branches
would have removed the server-side fast-forward-only backstop those
branches depend on. Separately, today's plain push already rejects every
race that could cost a concurrent writer their work: if dest moves from `X`
to `Y` while gitprism builds `N` on `X`, `Y` is not an ancestor of `N`, the
push is rejected, and decisions/0009's refetch-and-recompute runs. The only
race a lease additionally detects is the remote having moved to a commit
gitprism already had — harmless. This finding is unrelated to branch
authority and is recorded here only because the same review conflated the
two; no lease mechanism is adopted anywhere, for either branch type,
regardless of this decision's outcome.
**[Revised by decisions/0040: this paragraph's "no lease mechanism is
adopted anywhere" conclusion is revised for mirror-only force. The finding
that a lease with a correct expected value still force-pushes a divergent
history is unchanged and is why a lease stays banned as a fast-forward
substitute for round-tripped branches; 0040 uses the lease instead as
compare-and-swap concurrency control around a force already authorized by
branch authority and rewrite detection — a question this paragraph never
evaluated.]**

**Existing push call sites**, confirmed by reading `src/git.rs` and both
command modules — `git::push` takes no force flag today
(`src/git.rs:496-542`, doc comment: "Deliberately no `--force`"):

* `src/commands/sync.rs:509` — source→dest, `build_pending_dest_tip`'s
  result pushed to dest. `branch` here may or may not be in
  `config.branches` (decisions/0017: every source branch is discovered and
  mirrored, not just round-tripped ones).
* `src/commands/sync.rs:1404` — dest→source, `build_pending_source_tip`'s
  result pushed to source. `branch` here is always in `config.branches` —
  `run()`'s dest→source loop only ever iterates that list (confirmed also
  by the comment at `sync.rs:1430-1433`, "branch here is always named in
  config.branches ... no mirror-only case for this direction").
* `src/commands/resolve.rs:335` and `src/commands/resolve.rs:579` — both
  inside `resolve_source_to_dest`, pushing a resolved dest commit. Read
  `run_with_direction` (`resolve.rs:104-119`): `Direction::SourceToDest`
  accepts **any local source branch** (`repo.find_branch`), not just
  `config.branches` — unlike `Direction::DestToSource`, which does look the
  branch up in `config.branches` and errors if absent. So these two dest
  pushes are exactly as likely to target a mirror-only branch as sync's own
  `sync.rs:509`, and cannot be assumed round-tripped.
* `src/commands/resolve.rs:1324` — inside `finish` (the dest→source resolve
  path), pushing to source. Always round-tripped, for the same reason as
  `sync.rs:1404`: reached only through `Direction::DestToSource`, which
  `run_with_direction:104-111` already restricts to `config.branches`.

(The line numbers in the originating request — `sync.rs` ~430/~1218 — have
moved; corrected above. `resolve.rs` 335/579/1324 were accurate.)

# Decision

**Force semantics are decided by branch authority, not by which direction
is pushing.**

| Direction / branch type | Force allowed? | On divergence |
|---|---|---|
| source→dest, round-tripped | No | stop / operator |
| dest→source, round-tripped | No | stop / operator |
| source→dest, mirror-only | Yes, when mirroring rewritten source history | rewrite dest to source |
| dest→source, mirror-only | N/A | never synced back |

* **Round-tripped branches never accept a forced update, in either
  direction.** A non-fast-forward rejection means the two sides' histories
  diverged; that is an operator boundary, not something gitprism resolves
  by picking a side.
* **Mirror-only branches may be force-updated on source→dest**, because
  their dest history is a projection nothing ever imports back — an
  intentional source-side rewrite (rebase, amend, branch reset) is
  legitimately reflected onto dest by rewriting dest to match.
* **Force is a last resort, applied only after decisions/0009's bounded
  refetch-and-recompute is exhausted, never as a first response to a
  non-fast-forward rejection.** A mirror-only branch's non-fast-forward can
  still be a benign race — another clone legitimately advancing the same
  dest branch — and that must be incorporated, not clobbered. Recompute
  against dest's new tip first, exactly as today. If the source branch
  really was rewritten, recomputation reproduces the same non-fast-forward
  every retry, so the bounded retries cost a few round-trips and then force
  proceeds; if it was a benign race, recompute resolves it as it already
  does today, and force never triggers. Decisions/0009's mechanism is
  unchanged; this decision only defines what happens once its retries are
  exhausted, for mirror-only branches specifically.
  **[Superseded by decisions/0039: this whole bullet's "exhaust retries,
  then force" trigger is overturned. 0039 positively detects a rewrite via
  four direct conditions before any push is attempted; a benign race is
  still told apart from a real rewrite, but not by counting failed
  attempts. See the supersession note at the top of this document.]**
* **Force is opt-in and visible at the call site, not a separate helper.**
  `git::push` gains an explicit two-variant mode, e.g.:
  ```rust
  pub enum PushMode {
      FastForwardOnly,
      ForceMirrorOnly,
  }
  ```
  passed at every call site, so intent is declared where the push happens
  rather than inferred from which function got called. A separate
  `force_push` helper alongside the existing `push` was considered and
  rejected: it is easy to reach for by accident at a round-tripped call
  site, exactly the mistake an explicit required argument prevents.
  `PushMode::ForceMirrorOnly` performs the push with `--force` (or an
  equivalent explicit `+refspec`); `PushMode::FastForwardOnly` is today's
  existing behavior, unchanged.
* **Every call site above must be classified, not defaulted:**
  * `sync.rs:509` (source→dest) — mode depends on whether `branch` is in
    `config.branches` at the time of this run: `FastForwardOnly` if so,
    `ForceMirrorOnly`-eligible (after retries exhaust) if not.
    **[Superseded by decisions/0039: eligibility is a positively detected
    rewrite, not "after retries exhaust."]**
  * `sync.rs:1404` (dest→source) — always `FastForwardOnly`; never
    eligible for `ForceMirrorOnly`, since only round-tripped branches reach
    this path.
  * `resolve.rs:335`, `resolve.rs:579` (dest pushes, source→dest resolve)
    — same `config.branches` membership test as `sync.rs:509`. Must not be
    assumed round-tripped just because `resolve` is a human-driven path;
    `Direction::SourceToDest` accepts any local branch.
  * `resolve.rs:1324` (source push, dest→source resolve) — always
    `FastForwardOnly`, same reasoning as `sync.rs:1404`.
* **Role is decided by current config — an operator hazard.** A branch's
  authority is whichever category it falls into by testing membership in
  `config.branches` *right now*, at the time of that run — there is no
  separate, persisted "this branch is round-tripped" record. Removing a
  branch from `config.branches` silently converts a protected shared
  history into a force-eligible projection on its very next sync; adding
  one does the reverse. This is a real operator hazard, not a defect to
  design around here — reviewing a `config.branches` diff must be treated
  as a change to which branches gitprism may rewrite, not merely which
  branches round-trip.
* **The exhausted-retry message for a round-tripped branch's persistent
  non-fast-forward must name the branch, say the histories diverged, and
  hand reconciliation to the operator — without prescribing merge, rebase,
  or cherry-pick.** Choosing among those is exactly the human decision
  gitprism must not automate (`AGENTS.md`'s operator-intervention rule).
  Today's message at both `sync.rs:519`/`sync.rs:1422-1425` — "kept losing
  a fast-forward race after N retries" — states the symptom and gives no
  next step; it must be replaced with one that names `branch`, states that
  source and dest (or dest and source) diverged, and tells the operator to
  reconcile with ordinary git, leaving the method to them.
* **Guard tests are role-aware, not blanket.** Do not assert "force is
  never used" anywhere in the suite — that assertion is false for
  mirror-only source→dest and would have to be deleted the moment
  `ForceMirrorOnly` is implemented. Instead: a round-tripped push (either
  direction, either module) never emits `--force`, `--force-with-lease`, or
  a `+`-prefixed refspec; a mirror-only source→dest push, once its retries
  are exhausted against a genuinely rewritten source branch, may use
  `PushMode::ForceMirrorOnly`.
  **[Superseded by decisions/0039: the trigger is a positively detected
  rewrite, not exhausted retries — see the supersession note above.]**

# Why

* **Authority, not direction, matches the workflow's actual asymmetry.**
  [decisions/0017](0017-source-to-dest-mirrors-every-branch.md) already
  established that round-tripped and mirror-only branches are treated
  differently by scope (dest→source only touches `config.branches`); this
  decision extends the same distinction to force semantics, rather than
  inventing a second, direction-based axis alongside it.
* **Last-resort ordering reuses decisions/0009 rather than replacing it.**
  Recompute-first is already the correct response to *any* non-fast-forward
  rejection, benign or not — decisions/0009's own reasoning (rebasing a
  stale local build is itself a merge-shaped operation this project avoids)
  applies just as much to the mirror-only case. Force only changes what
  happens after recompute has already had its bounded chances and still
  disagrees with dest.
  **[Superseded by decisions/0039: force is triggered by positively
  detecting a rewrite, not by recompute's retries running out. Decisions/0009's
  recompute-first behavior for a genuine race is unaffected either way.]**
* **Explicit `PushMode` over a second helper function**, for the same
  reason decisions/0009 preferred recompute over rebase: don't hand-build a
  parallel path that can silently diverge from the primary one. A required
  enum argument makes every call site self-documenting and un-skippable in
  review; a same-signature `force_push` helper is one accidental find-
  replace away from being called where it shouldn't be.
* **Config-role-from-current-state is consistent with existing precedent,
  not a new pattern.** [decisions/0018](0018-branch-deletion-failure-modes.md)
  already re-derives a branch's merge status "from scratch, every time,"
  citing decisions/0004's "current state governs, not history." This
  decision's config-membership test is the same idiom applied to a
  different question; the operator hazard it creates (Consequences) is the
  same shape as any current-state-governs rule and is recorded rather than
  engineered around, matching the project's stated preference for operator
  intervention over novel automation.
* **The lease was rejected on its own facts, not by analogy to this
  decision.** Verified directly against git rather than assumed: a
  `--force-with-lease` with a correct expected value still reported
  `(forced update)` against a divergent remote. It is strictly weaker than
  the fast-forward-only default already in use, and the one race it adds
  detection for (remote moved to a commit gitprism already had) costs
  nothing to leave undetected. Recording this here, not as a separate
  decision, because the reviewer's proposal and this decision's mirror-only
  force are easy to conflate — a lease is concurrency control; this
  decision's force is a deliberate, config-scoped policy choice about
  which branches are allowed to be rewritten at all.
  **[Revised by decisions/0040: the lease's own facts here — a
  correct-value lease still reports `(forced update)` against a divergent
  remote — are unchanged, and still ban a lease as a fast-forward substitute
  for round-tripped branches. But this bullet's conclusion that the lease
  "adds no safety a deliberate, authorized mirror rewrite needs" is
  overturned: it never evaluated the lease as compare-and-swap against a
  concurrent dest advance happening in gitprism's own fetch-to-push window,
  which is exactly what 0040 uses it for.]**
* **Reconciling with requirements/0001's explicit constraint.** The
  `git-filter-repo` objection (`design/references/git-filter-repo.md`) is
  that rewriting changes every commit hash on *every run*, making
  force-push the permanent steady-state mechanism — every clone must reset
  or "recontaminate the rewrite on its next ordinary `pull && push`." That
  is not what this decision permits. The steady state for every branch,
  round-tripped or mirror-only, remains fast-forward: `ForceMirrorOnly`
  fires only in the rare case a human deliberately rewrote a mirror-only
  branch on source, and only after recompute has confirmed the divergence
  is real rather than a race.
  **[Superseded by decisions/0039: "only after recompute has confirmed" is
  the retry-escalation framing 0039 overturns — the rewrite is positively
  detected before any push or recompute is attempted. The surrounding point
  about force not becoming the steady-state mechanism still holds.]**
  A dest clone tracking a mirror-only branch
  still needs to reset after such an event, exactly as any git mirror does
  after an upstream rewrite — but that event is exceptional operator
  action on source, not something every sync invocation does. The
  constraint this decision must not violate is "force-push as the
  steady-state sync mechanism," and it does not become that.

# Consequences

* **dest→source is entirely unaffected.** Only round-tripped branches ever
  flow dest→source (decisions/0017); `sync.rs:1404` and `resolve.rs:1324`
  are always `FastForwardOnly` and never gain a force path.
* **decisions/0009 is unchanged in mechanism.** Refetch-and-recompute,
  bounded retries, and the `[rejected]`-porcelain race-detection idiom
  (its 2026-08-20 addendum) all stand exactly as written. This decision
  adds only what happens once those retries are exhausted, for mirror-only
  branches — it does not supersede 0009.
  **[Superseded by decisions/0039: what force is added *for* is a
  positively detected rewrite, not "once retries are exhausted." 0009's own
  mechanism is still unchanged, as stated.]**
* **No lease mechanism anywhere.** `--force-with-lease` is not adopted for
  round-tripped pushes (would remove the fast-forward backstop they
  require) or for mirror-only force (where the force is already outright
  and intentional — a lease's compare-and-swap adds no safety a deliberate,
  post-recompute force needs).
  **[Revised by decisions/0040: the round-tripped half of this bullet
  stands — a lease is still never adopted there. The mirror-only half is
  overturned: 0040 adopts
  `--force-with-lease=refs/heads/<branch>:<fetched-dest-oid>` for
  mirror-only force specifically, as compare-and-swap against a concurrent
  dest advance in the fetch-to-push window — a real safety gap an
  unconditional force cannot close.]**
* **Config-role hazard is a standing operational fact, not mitigated
  here.** Editing `config.branches` changes which branches gitprism may
  rewrite, not just which branches round-trip; this decision does not add
  a confirmation step, a second config field, or a warning for that edit —
  consistent with the project's default of operator intervention over
  novel automation, and left for a future decision if it becomes a real
  problem.
* **`git::push`'s signature changes** to take a `PushMode`, requiring every
  existing call site to be touched and classified (enumerated in Decision)
  rather than defaulting silently.
* **Tests the implementation commit must add:**
  **[Superseded by decisions/0039: the two mirror-only cases below are
  described here as "retries exhaust" / "within the retry bound," but 0039
  requires them to exercise its four-condition positive detection instead —
  see 0039's own, more detailed test list, which is what was actually
  implemented.]**
  * a mirror-only source→dest branch whose source history was genuinely
    rewritten: retries exhaust with the same non-fast-forward each time,
    then `ForceMirrorOnly` succeeds and dest ends at source's rewritten
    tip;
  * a mirror-only source→dest branch hitting a *benign* race (another
    clone legitimately advanced dest): recompute resolves it within the
    retry bound, and force is never invoked;
  * a round-tripped branch (either direction) whose non-fast-forward
    persists past the retry bound: the run fails with a message naming the
    branch and stating the histories diverged, without mentioning merge,
    rebase, or cherry-pick;
  * a role-aware guard test: round-tripped pushes (both `sync.rs` call
    sites, both `resolve.rs` round-tripped call sites) never emit
    `--force`, `--force-with-lease`, or a `+`-prefixed refspec; a
    mirror-only source→dest push is permitted to use
    `PushMode::ForceMirrorOnly`;
  * `resolve.rs:335`/`resolve.rs:579` each get a case exercising a
    mirror-only branch (not in `config.branches`) alongside their existing
    round-tripped coverage, confirming the mode is chosen by config
    membership rather than assumed from the fact that `resolve` is being
    run at all.

# Prior art

[decisions/0018](0018-branch-deletion-failure-modes.md) already cites
GitLab's push-mirror feature as the closest analogue to gitprism's
source→dest direction. Checked directly (same source GitLab docs page
already cited there) for this decision specifically: GitLab's push
mirroring **does force-update the mirror by default** when a ref diverges
— "if any ref (branch or tag) on the remote (downstream) mirror diverges
from the local repository, the upstream repository overwrites any changes
on the remote" — with an opt-in "Keep divergent refs" setting to suppress
it. This is real, shipped, documented precedent for force-updating a
mirror to match an authoritative upstream, on the same tool this project
already treats as the closest analogue for source→dest's shape.

No other tool already reviewed does this, and none was found to contradict
it: [josh](../references/josh.md) is built specifically to avoid ever
needing a force-push (filtering is reversible by construction);
[Copybara](../references/copybara.md) doesn't claim fast-forward-only or
force-update behavior either way; [git-subtree](../references/git-subtree.md)'s
push path is explicitly non-rewriting; [jujutsu](../references/jujutsu.md)
is relevant here only for its git-backend split, not sync semantics;
[git-filter-repo](../references/git-filter-repo.md) is the rejected shape
this decision is careful not to become (Why, above). No precedent found
among those five for or against; GitLab push mirroring is the one
on-point precedent, and it confirms rather than rules out this decision.
