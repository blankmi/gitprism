---
type: Decision
title: A discovered branch's unsafe-resume-point refusal is a per-branch halt, not a whole-run abort
description: The two remaining fatal anyhow::bail! sites reachable from source→dest's per-branch loop — dest_resume_point_for_branch's "isn't at a point this clone can safely build on" refusal, and graft_point's "no shared history" merge-base failure — are converted to decisions/0024/0037/0043's existing per-branch Outcome::Error + Ok(true) pattern for a branch outside config.branches. Round-tripped (config.branches) branches keep the fatal bail unchanged: dest→source really can leave independent content on those branches that a refusal must not silently paper over.
tags: [architecture, branches, error-handling]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-27T00:00:00Z }
---

# Context

[decisions/0024](0024-sync-warns-and-continues-past-a-mirror-only-branch-with-no-shared-history.md),
[decisions/0037](0037-branch-policy-mismatch-fails-closed.md), and
[decisions/0043](0043-mirror-only-branches-graft-onto-their-nearest-mirrored-ancestor.md)
each deliberately convert a structural surprise on a *discovered* branch
(decisions/0017: source→dest mirrors every branch on source, not just
`config.branches`) into a per-branch `Outcome::Error`/`Outcome::Warning`
that `run()` reports and moves past, rather than a fatal error that aborts
the whole invocation — an operator never asked gitprism to manage a branch
it merely discovered, so one such branch's problem shouldn't cost every
other branch, including every properly configured one, its sync this run.

A repository review (finding F-05) found two refusal sites in
`sync_pair_to_dest_with_key` (`src/commands/sync.rs`) that this pattern
missed:

1. The unconditional `None => anyhow::bail!("...isn't at a point this
   clone can safely build on...")` arm, reached whenever
   `dest_resume_point_for_branch` refuses and
   [decisions/0039](0039-mirror-only-source-rewrites-rebuild-the-projection.md)'s
   rewrite detection doesn't positively identify a rewrite either.
2. `graft_point`'s `repo.merge_base(source_tip, dest_tip)` failure — no
   merge-base at all between source and dest for this branch — propagated
   via `?` from inside `dest_tip_accounted_for`, through
   `dest_tip_is_accounted_for`, through `dest_resume_point_for_branch`'s own
   `?` at its call site, before the `None` arm above is ever reached.

Both are reachable for a branch outside `config.branches`: a dest-native
branch created independently of gitprism, sharing a name with a branch
source also independently created, with no shared history at all (case 2
above); or a discovered branch whose dest ref exists but is genuinely
behind or diverged from what this clone's source history supports (case 1).
Because `sync_pair_to_dest_with_key` is called from `run()`'s loop over
every branch discovered on source, sorted alphabetically, and the old
`anyhow::bail!` propagates via `run()`'s own `.with_context(...)?`, a
branch hitting either refusal starves every later-sorted branch —
including `main`, whenever its name sorts after the offending one — of its
own turn that run.

The existing refusal message also claims "either dest→source hasn't
reflected its content into source yet" — true for a round-tripped branch,
where dest→source genuinely could have (but hasn't yet) reflected
independent dest content back. Never true for a discovered, mirror-only
branch: [decisions/0017](0017-source-to-dest-mirrors-every-branch.md)
guarantees dest→source never runs for a branch outside `config.branches` at
all, so that clause cannot apply and misleads the operator toward a
non-existent remedy.

# Decision

Both refusal sites are converted to a per-branch halt — matching
decisions/0024/0037/0043's own pattern exactly (`reporter.complete(Outcome::Error,
...)` then `return Ok(true)`, `run()`'s existing `any_branch_halted`
accumulator) — **only for a branch absent from `config.branches`**. A
round-tripped branch keeps the fatal `anyhow::bail!` unchanged: dest→source
really can leave independent content on a `config.branches` branch this
clone doesn't yet recognize, and silently downgrading that to a per-branch
note would hide exactly the divergence an operator must resolve before any
further sync of that branch is safe.

**Site 1** (`dest_resume_point_for_branch` refusal): the `None` match arm
gains a `round_tripped` guard —

```rust
None if round_tripped => anyhow::bail!(/* unchanged message */),
None => {
    reporter.complete(Outcome::Error, branch, Direction::SourceToDest,
        round_tripped, Some(&unsafe_to_build_on_message(branch)));
    return Ok(true);
}
```

`unsafe_to_build_on_message` is new, dedicated wording for the discovered-
branch case that drops the "dest→source hasn't reflected its content yet"
clause entirely — it names the branch, states plainly that its dest history
and this clone's source history share no ancestry gitprism recognizes
(either a genuinely unrelated dest-native branch of the same name, or this
clone being behind), and gives the same remedy (fetch/pull source, or
reconcile manually if the histories are genuinely unrelated). The
round-tripped message is untouched — "dest→source hasn't reflected its
content yet" is a real, applicable explanation there.

**Site 2** (`graft_point`'s merge-base failure): `graft_point` changes from
`Result<Oid>` (erroring on no merge-base) to `Result<Option<Oid>>`
(`Ok(None)` on no merge-base) — the same "no positive answer, let the
caller decide" shape `dest_anchor_for_branch`'s own sibling-search already
uses for an unrelated candidate (`let Ok(cbase) = repo.merge_base(...) else
{ continue }`, decisions/0043). `dest_tip_accounted_for`'s Case 2 changes
its equality check accordingly (`graft_point(...)? == Some(dest_tip)`) and,
on `None`, simply falls through to Case 3 rather than erroring — which
correctly resolves to `DestTipAccountedFor::No` when nothing else matches,
flowing into `dest_resume_point_for_branch`'s existing `Ok(None)`, reaching
Site 1's now-converted arm above. No merge-base at all is no longer a
distinguishable third failure mode — it collapses into the same "not
accounted for" signal the rest of the safety check already produces,
resolved the same way.

The one place `graft_point`'s `None` still means a real, unexpected problem
is `dest_resume_point_for_branch`'s own fallback (`newest_source_marker`
found nothing, so the boundary must be the graft) — reachable only once
`dest_tip_is_accounted_for` has already returned `true` via a genuine
gitprism marker, which is only possible if real shared history exists.
There, `graft_point`'s `None` would mean that invariant broke; it still
fails loudly via `.context(...)?`, unchanged in effect from before this
decision, just re-expressed against the new `Option`-returning signature.

`run()`'s own aggregate halted-branch message (the one printed if
`any_branch_halted` after every branch has had its turn) is extended to
name this third halt reason alongside decisions/0037's policy mismatch and
decisions/0043's ambiguous anchor.

# Why

* **Matches existing, already-decided precedent exactly** — decisions/0024,
  0037, and 0043 already establish that a discovered branch's structural
  surprise is per-branch, not whole-run; this decision doesn't invent a new
  pattern, it closes two call sites the pattern's rollout missed.
* **The round-tripped/discovered split is the same split this codebase
  already draws everywhere else** (decisions/0038's authority invariant,
  decisions/0043's `round_tripped` boolean already computed once per branch
  and threaded through every `reporter.complete` call) — reusing it here
  keeps the codebase's one authority test singular rather than adding a
  second, parallel notion of "which branches get the safety net."
* **`graft_point` returning `Option` instead of erroring mirrors the
  existing idiom** `dest_anchor_for_branch`'s own candidate search already
  uses for exactly the same underlying git operation (`repo.merge_base`)
  failing for the same reason (no shared history) — one way to express "no
  merge-base," not two.
* **Fixing the misleading message is a correctness fix, not polish**: an
  operator who acts on "dest→source hasn't reflected its content yet" for a
  branch dest→source never touches is being pointed at a nonexistent
  remedy.

# Consequences

* A discovered branch's unsafe-resume-point refusal, for any reason, no
  longer starves later-sorted branches (including `main`, whenever it sorts
  after the offending branch) of their own sync in the same run. The
  overall `sync` invocation still exits non-zero when this happens
  (`run()`'s existing `any_branch_halted` aggregate bail, decisions/0037's
  mandate that a halt must be loud on both the human-readable and the exit-
  status channel) — this decision changes *how much of the run* one
  branch's refusal costs, not *whether* the run reports failure.
* A round-tripped branch's identical refusal is completely unaffected —
  still an immediate fatal `anyhow::bail!`, still stops the run right
  there, unchanged wording.
* `graft_point`'s signature change (`Result<Oid>` → `Result<Option<Oid>>`)
  is internal to `src/commands/sync.rs`; both of its call sites are updated
  in the same commit, no other module depends on it.
* **Tests the implementation commit adds:** a dest-native branch sharing a
  name with a discovered source branch, genuinely no shared history — that
  branch reports `Outcome::Error` and halts, a later-sorted `config.branches`
  branch still gets processed in the same run, and the overall run fails
  (decisions/0037's non-zero-exit precedent, applied here too). A unit test
  on the new message-building helper confirms it names the branch and never
  claims dest→source could have applied. The existing shallow/non-shallow
  "boundary object missing" tests (decisions/0039's addendum) are updated
  from asserting a fatal `Err` to asserting the per-branch halt
  (`Ok(true)`), since `feature-x` in those fixtures is mirror-only.

# Prior art

Not independently researched; this decision applies
[decisions/0024](0024-sync-warns-and-continues-past-a-mirror-only-branch-with-no-shared-history.md)'s
own prior-art finding (GitLab push-mirror's per-mirror, not whole-job,
failure handling) to two call sites that finding's original implementation
didn't reach.
