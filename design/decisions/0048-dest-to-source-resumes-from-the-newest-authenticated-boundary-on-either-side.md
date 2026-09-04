---
type: Decision
title: dest-to-source resumes from the newest authenticated boundary on either side
description: Fixes CODE-001 (docs/2026-09-02_REPOSITORY_REVIEW.md, section 3) — a branch cut from a round-tripped branch after `setup`, later added to `config.branches`, only inherited the parent's `Setup` graft, so `pending_dest_commits` replayed the parent's own mirrored commits and hard-stopped on a phantom conflict, with no supported path since `setup` refuses a branch that already carries an inherited marker. The dest→source boundary is now the end of the longest contiguous prefix of dest's first-parent line, starting immediately after the existing `Setup`/`DestToSource` marker boundary (B1), for which every commit is proven **represented** in source's current tip: a commit is represented when either it is itself a valid `SourceToDest` marker whose own source counterpart is reachable from source's tip (case 1), or a self-verified `DestToSource` marker reachable from source's tip — regardless of that marker's own recorded branch — names that exact dest commit (case 2); a `SourceToDest` marker whose own counterpart isn't reachable can still be represented by case 2, so the two cases are not a partition by commit shape — only a dest-native or invalid/unauthenticated marker commit is restricted to case 2 alone, because it can never satisfy case 1 at all. The walk stops at the first unrepresented commit; the boundary is the commit before it, which may be B1 itself, a qualifying `SourceToDest` marker, or an accepted dest-native commit. Supersedes two insufficient predicates found during implementation, before either was committed: a bare ancestor check on each marker's own counterpart (blind to dest-native content skipped when a branch is mirror-aliased onto another branch's marker), and a blanket "any dest-native commit disqualifies" rule (cannot distinguish an imported dest-native commit from an unimported one, reintroducing CODE-001's own bug for any branch descended from a parent with import history). A 2026-09-03 addendum (CODE-001 step 5) extends this same represented-prefix predicate to source→dest's own push-safety gate, so a branch promoted into `config.branches` after being mirror-only can round-trip both directions in one run — plus three review-found corrections to that extension: two before either was committed (the safety check's "is there a branch-scoped marker anywhere on this line" half was replaced with "is the *newest* marker on the line branded for this branch", after a foreign-branded marker sitting above a stale branch-scoped one was shown to grant safety on the stale boundary; and B1's first-parent-reachability precondition was made a per-branch `Ok(false)` instead of a whole-run-aborting `Err` for a discovered branch), and one found by a second review, also before commit: that "newest marker" check was itself still wrongly bounded to the B1..dest_tip range rather than searched over dest's whole first-parent line, letting a newest mirror marker that a dest→source import had advanced B1 past go undetected and grant safety on a stale boundary — fixed by searching the whole line instead. A fourth correction, found by an external review after commit (2026-09-04): that gate ran only on the represented-prefix accounting path, so an ordinary same-branch import naming dest's tip bypassed it via `dest_tip_accounted_for`'s case 3 — fixed by applying the gate after either accounting path accepts dest's tip.
tags: [architecture, branches, markers, dest-to-source]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-09-03T00:00:00Z }
---

# Context

`pending_dest_commits` (`src/commands/sync/mod.rs:1649-1683`) computes
dest→source's resume boundary as `newest_dest_marker(repo, source_tip, branch,
key)` (`src/commands/sync/marker_scan.rs:86-97`), a thin wrapper over
`scan_for_dest_marker`: walk source's first-parent history from `source_tip`
and return the newest commit carrying a `Setup` marker (any branch — the setup
graft, [decisions/0006](0006-setup-uses-real-shared-history.md), is
deliberately inherited by every branch cut from it) or a `DestToSource`
marker for `branch` exactly ([decisions/0003](0003-mapping-state-in-commit-trailers.md),
`marker::verify` with the branch check). Once found, `pending_dest_commits` confirms that
boundary is still an ancestor of (or equal to) `dest_tip` via
`repo.graph_descendant_of` before trusting it to scope `pending_commits`
(dest is fast-forward-only, requirements/0001), then drops any commit already
loop-prevented by a `SourceToDest` marker for `branch` (decisions/0003).

A branch cut from a round-tripped branch (e.g. `main`) after `setup` has run
carries only `main`'s inherited `Setup` graft in its first-parent history —
nothing else, because it is new. `newest_dest_marker` finds that graft, so
the boundary is dest's tip *at setup time*, not at branch-creation time. Every
dest commit on the new branch's first-parent line since then becomes
"pending" — including commits gitprism itself mirrored onto dest from `main`
(source→dest, [decisions/0017](0017-source-to-dest-mirrors-every-branch.md)),
each carrying a `Gitprism-Branch: main` marker.
`pending_dest_commits`'s loop prevention checks `marker::verify(commit,
branch, [SourceToDest], ..., key)` against the *new* branch's own name
(decisions/0003's per-branch scoping), so `main`'s markers don't exempt them.
Each is 3-way merged against the current source tree by
`build_pending_source_tip`; any line touched twice since conflicts, and the
branch hard-stops without ever reaching the operator's real dest commit.

There is no supported recovery. `setup` (`src/commands/setup.rs:229-241`)
refuses to graft a branch whose local tip already carries a `Setup` or
`DestToSource` marker — deliberately, since `setup` is a one-time step
(decisions/0006, [decisions/0012](0012-config-versioned-in-source.md)) and
re-running it against its own prior
output must fail rather than silently reconcile. A branch promoted to
`config.branches` after being mirrored always has exactly this inherited
marker, so `setup` cannot re-graft it and `sync` mis-resumes it. Reproduced by
execution: `docs/2026-09-02_REPOSITORY_REVIEW.md`, CODE-001, with the
regression test in its Appendix.

[Decisions/0046](0046-dest-anchors-come-from-exact-authenticated-mappings.md)
solved the equivalent problem for source→dest — an exact,
authenticated mapping index reconstructed from marker state rather than
comparing branch topology — but that index is built only after dest→source
has already run (0046, "Configured dest-to-source processing remains first");
dest→source itself has no equivalent to anchor on.

Implementing this decision (CODE-001 step 4) went through two more rounds,
both found wrong before either was ever committed. The first draft's B2
predicate — a `SourceToDest` marker commit, self-verified against its own
recorded branch, whose own source counterpart is an ancestor of `source_tip`
— is not sufficient by itself: it proves the *candidate's own* counterpart is
reachable, not that everything on this branch's dest history beneath it was
ever imported for this branch. A branch mirror-aliased onto another branch's
own mirror commit (an anchor decisions/0046's nearest-exact-mapping
resolution can select, made self-accounting for the new branch by
decisions/0044's own-branch alias marker) can pass that check while a
dest-native commit sitting underneath was never imported *for it*. A
same-day fix attempted to close that gap by disqualifying any B2 candidate
with a dest-native commit anywhere between it and B1. That over-corrected:
"dest-native commit" cannot
distinguish a commit source has already imported (via some marker reachable
from `source_tip` that isn't itself on this branch's dest line) from one it
hasn't, so it stops the walk at the first native commit unconditionally —
reintroducing, for any branch ever descended from a parent with import
history, exactly the phantom-resume symptom this decision exists to fix. The
model below (the project owner's own specification) replaces both attempts
with a per-commit proof rule that tells the two cases apart.

# Decision

The dest→source boundary for branch `B` is the end of the **longest
contiguous prefix of dest `B`'s first-parent line, starting immediately after
B1**, for which every commit is **represented** in source `B`'s current tip
(`source_tip`).

**B1** is unchanged: the dest commit named by the newest `Setup`/
`DestToSource` marker on source `B`'s first-parent line (`newest_dest_marker`,
`scan_for_dest_marker`). It remains a precondition, checked first and
unconditionally, exactly as `pending_dest_commits` enforces today: if B1 is
missing from the local object database, or `graph_descendant_of(dest_tip,
B1)` is false (and `B1 != dest_tip`), refuse with today's "isn't an ancestor
of dest's current tip" message before any further walk.

Only once B1 is confirmed does a second, bounded walk of dest `B`'s
first-parent line run, from the commit immediately above B1 up toward
`dest_tip`, testing each commit in turn for representation.

Locating B1 on that first-parent line is itself part of this walk, and it
can fail even after the precondition above has passed: the B1 precondition
is a full-ancestry `graph_descendant_of` check, but the walk that has to
find "the commit immediately above B1" moves first-parent-only, and
decisions/0019's documented limitation (a tracked branch must stay
first-parent of its own merges for a marker scan to see it) applies to this
walk exactly as it already applies to `newest_dest_marker`'s own scan. A B1
that is a real ancestor of `dest_tip` only via a non-first-parent merge is
never reached by this walk. If dest `B`'s first-parent line is exhausted —
its root commit reached, or `MAX_MARKER_SCAN_COMMITS` hit — without ever
encountering B1 itself, this is a clear, named refusal citing
decisions/0019's first-parent limitation, not a raw walked-off-the-end
error and not a silent fallback to some other boundary.

**A dest commit `D` is represented in `source_tip` when either:**

1. `D` is a valid `SourceToDest` marker, self-verified against its own
   recorded branch (`marker::verify_self`, not necessarily `B`'s own name),
   whose source counterpart is an ancestor of, or equal to, `source_tip`
   (`repo.graph_descendant_of`). A counterpart oid absent from the local
   object database (e.g. a sibling branch's source commit this clone never
   fetched) simply disqualifies `D` from case 1 — it does not end the walk,
   halt the run, or fall back to case 2 on its own; `D` may still qualify
   via case 2 below; or
2. A valid, self-verified `DestToSource` marker — self-verified the same way,
   against its own recorded branch, whatever that is — reachable from
   `source_tip` on source's first-parent history (decisions/0019) carries a
   `Gitprism-Dest-Commit` trailer naming `D`'s exact oid, regardless of that
   marker's own recorded branch.

(Case 2 deliberately names `DestToSource` only, not `Setup`: a `Setup` graft
is already B1's own baseline or below it — `setup` runs once
(decisions/0006, 0012) — so a case-2 search above B1 would never find one
that case 1 or B1 itself hasn't already accounted for.)

| Commit between B1 and the proposed boundary | Required proof |
| --- | --- |
| `SourceToDest` marker | Case 1 (its own counterpart reachable from `source_tip`), **or** case 2 (an inherited marker reachable from `source_tip` names this exact dest commit) |
| Dest-native commit (no marker at all) | Case 2 only |
| Invalid or unauthenticated marker (fails `verify_self`) | Treated as dest-native: case 2 only |

The walk stops at the **first commit represented by neither case**. The
boundary is the commit immediately before it — which may be:

* B1 itself, if the very first commit above B1 is already unrepresented;
* a qualifying `SourceToDest` marker — case 1 (today's B2) if its own
  counterpart is reachable, or case 2 if it isn't but an inherited marker
  names it anyway; or
* an accepted **dest-native commit**, proven only by case 2 (it can never
  satisfy case 1, having no `SourceToDest` counterpart of its own) — a
  boundary shape the earlier B1/B2-only formulation never named in its own
  right.

If every commit up to `dest_tip` is represented, the boundary is `dest_tip`
itself.

Both the outer walk (dest's first-parent line, B1 to `dest_tip`) and each
case-2 inner search (source's first-parent line, from `source_tip`) are
independently bounded by `MAX_MARKER_SCAN_COMMITS`
([decisions/0032](0032-bounded-repository-controlled-data.md)), the same
static limit every other marker scan in this codebase uses. Reaching either
limit before a definite answer is a limit error, not a guess.

Loop prevention inside `pending_dest_commits` — filtering out a pending
commit that itself carries a `SourceToDest` marker for `branch` — stays
scoped to `branch`'s own name, unchanged. This decision only widens the
*boundary* the walk can name; it does not change which pending commits are
dropped once the boundary is found. Widening loop prevention itself to
`verify_self` (accepting any branch's `SourceToDest` marker, not just `B`'s
own) would be a different, wrong fix: a customer fast-forwarding dest
`release` into dest `main` must still import `release`'s independent content
into source `main`, and that content arrives on `main`'s first-parent line
carrying `release`'s own `Gitprism-Branch` marker — `verify_self` loop
prevention would recognize that marker and silently drop the content instead
of reflecting it back.

# Why

* **Branch-agnostic authentication, branch-specific reachability.** This is
  the same invariant decisions/0043's 2026-08-27 addendum (F-04) already
  settled for the mirror-image problem: a branch-scoped `DestToSource`
  marker's HMAC proves gitprism genuinely wrote it, for *some* branch, at
  some point (`verify_self` — branch-agnostic authentication); it does not
  by itself prove that *this* branch's history has ever seen the commit it
  names as already imported. Only an actual reachability check supplies the
  missing, branch-specific half — there, whether the marker is an ancestor
  of the branch being processed (`loop_prevented`, `src/commands/sync/mod.rs`,
  called only against commits the caller's own walk already found on that
  branch's history — not a check over the marker's own recorded branch,
  which would prove nothing about the branch actually being processed);
  here, case 1's `graph_descendant_of(source_tip, counterpart)` or case 2's
  search for an inherited marker reachable from `source_tip` specifically. A
  marker that authenticates cleanly but sits on unrelated history does not
  count in either decision.
* **Worked example: why case 1 alone is not enough.**
  ```
  source: s3 ── M(C0)
  dest:   C0 ── D(s3)
  ```
  Dest→source imports native commit `C0`, writing `M(C0)` on source *after*
  `s3` already exists there. Source→dest later mirrors `s3` onto dest as
  `D(s3)`, built directly on top of `C0` — decisions/0046's
  nearest-exact-mapping anchor resolution (self-accounted for via
  decisions/0044's own-branch alias marker) can place a new or rebuilt
  branch's mirror exactly here, reusing another branch's own mapping to `s3`
  rather than writing this branch's from scratch. `D(s3)` is a
  genuine, self-verified `SourceToDest` marker naming `s3`: case 1's
  ancestor check on `D(s3)` alone passes as soon as some `source_tip`
  descends from `s3`, regardless of whether that same `source_tip` also
  descends from `M(C0)`. `D(s3)` naming `s3` is not itself proof that a
  branch descending from `s3` also contains `M(C0)` — those are two
  independent facts about two different commits, and `s3` can be reached by
  a path that never passed through `M(C0)`. Trusting case 1 alone would then
  resume from `D(s3)` and skip `C0` forever, exactly the cross-branch
  mirror-alias failure the first implementation round missed. Case 2
  supplies the missing, exact, branch-specific proof: not "is `D(s3)`'s own
  counterpart reachable" but "does something reachable from *this*
  `source_tip` name `C0` itself."
* **Why round 2's "any dest-native commit disqualifies" over-corrects.**
  If `M(C0)` is created *before* `s3` — release cut from `s3` before `M(C0)`
  exists — `M(C0)` is genuinely not reachable from that release's
  `source_tip`, and the frontier correctly stops before `C0`: round 2 got
  this timing right, because it happens to agree with case 2 here. But if
  `M(C0)` is created *after* `s3` and is reachable from `source_tip` (the
  release was cut later, or its source line merges from a point past
  `M(C0)`), `C0` is genuinely represented — case 2 proves it — and the
  frontier must advance through `C0` and `D(s3)`. Round 2's rule cannot make
  this distinction: it disqualifies `C0` unconditionally, for being
  dest-native at all, without ever asking whether it was actually imported.
  The result is that `pending_dest_commits` resumes from B1 and replays
  `C0` (and everything above it) as new commits against the current source
  tree — the exact phantom-conflict failure CODE-001 exists to fix,
  reintroduced for the plan's own headline scenario: any branch descended
  from a parent that has imported dest-native content. Round 2's
  candidate-only formulation also has no answer at all when `dest_tip`
  *is* `C0` with nothing later — it only ever asks "does a later B2
  candidate's own run stay unbroken," so it would still report B1 as the
  boundary and replay an already-imported commit; the represented-prefix
  walk here treats `C0` itself as a boundary the moment it is proven
  represented, with or without anything above it.
* **Dest's first-parent line is the only place both proof shapes are
  ordered against each other.** Case 1's proof (does a marker's own
  counterpart chain to `source_tip`) and case 2's proof (does something on
  *source's* history name this dest commit) are each evaluated on a
  different side, but only a single revwalk of dest's own first-parent
  line, oldest-to-newest from B1, can ask "how far does the *contiguous*
  proven prefix reach" — the property the boundary is actually defined by.
* **`verify_self` plus `graph_descendant_of` are Git/decisions/0046's
  existing primitives, reused, not a new mechanism.** Case 1 is unchanged
  from the first implementation round. Case 2 reuses the identical pair —
  `verify_self` to authenticate a marker regardless of its recorded branch,
  `graph_descendant_of` (via plain reachability from `source_tip`, since the
  search itself already restricts the candidate set to commits reachable
  from `source_tip`) to confirm it — applied to the *reverse* lookup
  direction: instead of asking whether a given marker's counterpart is
  reachable, it asks whether anything reachable names a given dest oid.
  Per AGENTS.md's "ask whether Git already provides a safe primitive"
  rule, no new mechanism is introduced for either case.
* **Rejected: deriving the boundary from the decision-0046 mapping index.**
  That index is reconstructed once per run, but only *after* dest→source has
  already run for every configured branch — 0046's own "Configured
  dest-to-source processing remains first" ordering, chosen so source→dest's
  anchors see dest→source's output. Making dest→source depend on the same
  index would require reordering the run (breaking that guarantee, and
  coupling dest→source to PERF-001's fetch model along the way) or building a
  second, earlier index — strictly more machinery than a direct, bounded scan
  of the one branch's own dest history, which needs neither.

# Consequences

* This is a real walk, not a single check, and its cost is higher than the
  first implementation round's "no new asymptotic cost" claim, which no
  longer holds now that case 2 exists. The outer walk over dest's
  first-parent line (B1 to `dest_tip`) is unchanged in shape — bounded by
  `MAX_MARKER_SCAN_COMMITS`, the same range `pending_commits`'s own revwalk
  already traverses once the boundary is found. But each commit that fails
  case 1 (every dest-native or invalid-marker commit, and any
  `SourceToDest` marker whose own counterpart isn't reachable) requires an
  *independent* case-2 search of `source_tip`'s first-parent history, itself
  bounded by `MAX_MARKER_SCAN_COMMITS`. In the worst case — a long dest
  prefix of dest-native commits, each needing its own case-2 search — total
  work is the product of the two bounds, not their sum. This decision does
  not mandate a specific mitigation, but an implementer should expect to
  need one for realistic branch counts and history lengths: e.g. a single
  forward scan of `source_tip`'s first-parent history, built once per
  boundary computation, recording every `DestToSource` marker's named dest
  oid into a set, turns every case-2 check for that computation into O(1)
  against it — reducing the total cost back to the sum of the two bounds.
  Whether and how to do this is an implementation concern for CODE-001 step
  4's rework, not a change to the rule itself.
* [Decisions/0019](0019-marker-scans-are-first-parent-only.md)'s documented
  limitation (marker scans assume the tracked branch stays first-parent of
  its own merges) applies to *both* scans this decision uses: the outer walk
  over dest's own first-parent line (as it already did for
  `newest_dest_marker`'s source-side walk and `newest_source_marker`'s
  dest-side walk), and now also each case-2 search over source's
  first-parent line — the same accepted tradeoff `scan_for_dest_marker`
  already carries for B1, exercised again per case-2 lookup rather than
  once. No new exposure, but exercised more often.
* `setup`'s refusal of a branch whose local tip already carries an inherited
  `Setup`/`DestToSource` marker (`src/commands/setup.rs:229-241`) is
  unchanged — `setup` remains a one-time step (decisions/0006, 0012) — but it
  now has a real supported path on the other side: `sync` correctly resumes
  such a branch instead of mis-resuming it, so promoting a mirrored branch
  into `config.branches` after the fact is no longer a dead end — a genuine
  round trip in both directions in one run (CODE-001 step 5), not merely
  dest→source.
* `pending_dest_commits`'s doc comment, and `newest_dest_marker`/
  `scan_for_dest_marker`'s, need updating to describe the boundary as the end
  of a represented prefix past B1, not B1 *or* a single qualifying B2
  commit.
* `gitprism resolve`'s dest→source path, which calls `pending_dest_commits`
  directly (`resolve.rs:1171`, `:1258`), picks up the fix with no change of
  its own — resolve and sync must never disagree about which dest commit is
  next ([decisions/0008](0008-ship-resolve-helper.md)'s original invariant).
* Even with the memoized case-2 set, each case-2 miss still bails at
  `MAX_MARKER_SCAN_COMMITS` on source's first-parent history — and every
  ordinary dest-native commit above B1 is a case-2 miss. Before this
  decision, dest→source needed only B1 to be within
  `MAX_MARKER_SCAN_COMMITS` of `dest_tip`; now it also needs source's
  first-parent line, walked from `source_tip`, to be within that same bound
  — a hard-failure envelope this decision introduces, not merely a cost
  increase. Fails closed, as every other bounded scan in this codebase does,
  but an implementer should not read the memoization mitigation above as
  removing this ceiling.

# Rejected alternatives

* **Round 1: a bare ancestor check on each candidate's own counterpart, with
  no dest-native check at all.** Insufficient, not merely simpler — the
  worked example above (`D(s3)` naming `s3`, with `M(C0)` unreachable from
  some `source_tip` that still descends from `s3` by another path) shows a
  case-1-only check accepting a boundary that silently skips an unimported
  dest-native commit forever.
* **Round 2: disqualify any B2 candidate with a dest-native commit anywhere
  between it and B1.** Not merely too strong — it cannot tell an *imported*
  dest-native commit from an *unimported* one, so it stops the frontier at
  the first native commit unconditionally. For any branch descended from a
  parent that has ever imported dest-native content — the plan's own
  headline scenario — this reintroduces CODE-001's exact symptom: the
  frontier stays behind already-imported content and `pending_dest_commits`
  replays it, hard-stopping on a phantom conflict. It also has no way to
  name a bare dest-native commit as a boundary in its own right, so
  `dest_tip == C0` with no later marker still wrongly falls back to B1.
* **Derive the boundary from the decision-0046 mapping index.** See "Why"
  above — the index is built after dest→source runs, by 0046's own design.

# Test scenarios

Four scenarios, extending `docs/plans/2026-09-02/CODE-001-dest-to-source-boundary.md`'s
own scenario table (continuing its numbering) — required before the
represented-prefix model is implemented (CODE-001 step 4's rework):

| # | Setup | Expected boundary | Expected pending |
| --- | --- | --- | --- |
| 7 | Branch cut **before** `M(C0)` exists: dest `release` carries native `C0`; `release`'s `source_tip` never reaches any commit importing `C0` (its source line was cut, or last touched, before dest→source ever wrote `M(C0)` on `main`) | B1 (`C0` fails case 2 — no inherited marker reachable from this `source_tip` names it) | `[C0, ...]` — `C0` and everything above it on dest's line |
| 8 | Setup grafts `release` (B1). Dest `release`'s first-parent line: `B1 ── C0 ── D(s3)` — `C0` a customer commit added directly to dest, later imported into source as `M(C0)` on `release`'s own first-parent source line (reachable from `release`'s `source_tip`, whether imported before the branch was cut or merged in since); `D(s3)` is `release`'s own `SourceToDest` marker for source commit `s3` — either its own organic mirror, or, if `s3` was already mirrored on a sibling branch, decisions/0044's own-branch alias marker built on the decisions/0046 anchor (so `release`'s dest ref is never literally another branch's own marker commit) | `D(s3)` (`C0` represented via case 2; `D(s3)` represented via case 1) | `[]` |
| 9 | Setup grafts `release` (B1). Dest `release`'s first-parent line: `B1 ── C0`, and `dest_tip` **is** `C0` itself — a customer commit added directly to dest, imported into source as `M(C0)` on `release`'s own first-parent source line and reachable from `source_tip`; no later mirror or marker commit at all (a bare dest-native commit carries no gitprism marker to begin with, so it needs no decisions/0044 alias) | `C0` (`dest_tip` itself, not B1) | `[]` |
| 10 | A `SourceToDest` marker `D(sX)` on dest `release`'s line names a real, locally present `sX` that is reachable only from a **different** branch's source tip, not `release`'s own `source_tip` | the commit immediately before `D(sX)` (case 1 fails — not an ancestor of *this* `source_tip`; case 2 fails — no inherited marker reachable from this `source_tip` names `D(sX)`'s own oid) | `[D(sX), ...]` — not silently treated as already-imported |

Scenario 7 confirms round 1 remains correct where it always was — including
when the replayed prefix above `C0` also contains a later `SourceToDest`
marker for content `source_tip` already has (e.g. a mirror `D(s3)` for an
`s3` reachable from `source_tip`, sitting above `C0` on `release`'s dest
line, if the branch cut happened to still include one): `build_pending_source_tip`
(`src/commands/sync/mod.rs`) 3-way merges it against the growing source
chain to a clean, empty-diff result, and, per that function's own documented
asymmetry with source→dest's skip rule, still builds a real (content-
identical) marker commit for it — it is neither silently dropped from
`pending` nor reported as a conflict merely for merging to no change.
Scenario 8 confirms round 2's over-correction is gone; scenario 9 confirms a
bare dest-native commit is a valid boundary in its own right; scenario 10
confirms an authenticated, self-verified marker is not automatically trusted
without branch-specific reachability (the "Why" section's first bullet).

# Addendum (2026-09-03): source→dest's own push-safety gate also consults this decision's represented-prefix predicate

## Context

CODE-001 step 5 audited the one remaining `newest_dest_marker` caller,
`anchor::dest_tip_accounted_for`'s case 3 — source→dest's own "is dest's tip
safe to build on" check (this decision, as originally written, covered only
dest→source's own boundary). A promoted branch whose dest→source pass finds
`dest_tip` represented purely via this decision's case 2 (an inherited
marker, reachable from `source_tip`, naming `dest_tip` exactly, with no
case-1 marker of `dest_tip`'s own) writes *nothing* new to source, so B1
never moves — and case 3, comparing only against the unmoved B1, then
refused the *same run's* source→dest pass for the same branch as an
unrecognized state. The branch round-tripped dest→source but not both
directions in one run — the exact gap `setup`'s own refusal
(`src/commands/setup.rs:229-241`) was supposed to have a real path around,
via `sync`, once this decision landed.

## Decision

A separate function, `anchor::dest_tip_represented_in_source`, consulted
only by `dest_resume_point_for_branch` (source→dest's safety question)
alongside the unchanged, branch-scoped `dest_tip_accounted_for`. `dest_tip`
is additionally safe to build on when **both**:

1. This decision's own represented-prefix walk (`dest_to_source_boundary`,
   reused verbatim, not reimplemented) resolves to `dest_tip` itself — every
   commit between B1 and `dest_tip` is individually proven represented in
   `source_tip`; and
2. The **newest** self-verified `SourceToDest` marker anywhere on the
   **whole** of dest's first-parent line from `dest_tip` (bounded only by
   `MAX_MARKER_SCAN_COMMITS`, not by B1) — whichever branch's own scan
   happens to find it, if any — is branded for this branch, or there is no
   such marker on the line at all. Not bounded to B1: see "Corrected after
   this addendum was committed" below — a B1-bounded version of this exact
   check (as this addendum originally specified) was later found unsafe,
   because B1 is advanced by dest→source imports this check never consults,
   so the newest mirror marker actually on the line can sit below it.

Locating B1 for condition 1, and the newest marker for condition 2, are each
first checked for first-parent reachability before either walk runs (the
same decisions/0019 precondition this decision's own boundary walk already
enforces); failing that precondition resolves to `Ok(false)` — not an error
— exactly like every other "not accounted for" shape this function returns.
Only exceeding `MAX_MARKER_SCAN_COMMITS` remains a genuine hard error
(decisions/0032).

Condition 2 is deliberately not "does `newest_source_marker(dest_tip,
branch)` find *something*" — see "Rejected mid-flight, before either was
committed" below for why that disjunct is unsafe.

## Why

* **Not folded into `dest_tip_accounted_for` directly.**
  `dest_tip_accounted_for` is also `branch_scoped_dest_tip`'s fixpoint check
  (decisions/0044's F-02: does a new branch anchored on a sibling's dest
  commit need its own branch-scoped marker, or does that commit already
  carry the new branch's own identity) — a question that must stay
  branch-scoped. This decision's case 1/case 2 are deliberately
  branch-*agnostic* (`marker::verify_self`); folding the represented-prefix
  walk into case 3 directly let a branch anchored on a *sibling's* own
  marker commit (e.g. a task branch forked from a mirror-only feature branch
  with no commits of its own, decisions/0043) wrongly conclude the sibling's
  marker already counted as its own, so it never got a branch-scoped marker
  of its own — silently breaking every later branch-scoped scan
  (`newest_source_marker`, `newest_dest_marker`) that depends on one
  existing. Caught by execution, not inspection: two existing tests
  regressed
  (`sync_pair_to_dest_gives_a_branch_with_no_commits_of_its_own_a_branch_scoped_marker`
  directly;
  `mirror_only_branch_later_added_to_config_branches_only_reflects_dest_native_commits`
  downstream, once a promoted branch's own mirror silently reused a
  sibling's marker in an earlier run and a later `newest_source_marker` scan
  for it found nothing, falling back to the graft and replaying
  already-mirrored source content into a same-line conflict).
  `mirror_only_rewrite_detected` (decisions/0039) also calls
  `dest_tip_accounted_for` directly and is deliberately **not** widened
  either: it requires `ViaPriorGitprismSync` (a genuine prior branch-scoped
  sync) to license a decisions/0039 force-push rebuild, and a round-tripped
  branch — the only shape this widening's own scenario applies to — never
  reaches that arm regardless (it is gated on `!round_tripped`,
  `src/commands/sync/mod.rs`).
* **Condition 1 alone is not enough.** Condition 1's own represented-prefix
  walk must not outrun what `dest_resume_point_for_branch`'s other half,
  `newest_source_marker`, can actually resume from. That scan is
  branch-scoped by design (`marker::verify` against `branch` exactly; unlike
  case 1/case 2, no `verify_self` exemption for `SourceToDest`), so a dest
  ref parked entirely on a *sibling* branch's own mirror chain — scenario
  2's own shape (dest `hotfix` cut from dest `main` at `main`'s own mirror
  commit, nothing of `hotfix`'s own on either side yet) plus a same-run
  source→dest pass, which scenario 2's existing test never exercised — can
  be fully represented by condition 1's walk alone (case 1 doesn't care
  whose marker it is) while carrying no `hotfix`-branded marker anywhere on
  its own line. `newest_source_marker` would then find nothing and fall back
  to the graft, replaying `main`'s own already-mirrored source content onto
  `hotfix`'s dest tip as if it were new — a same-line conflict, or silent
  duplication for non-conflicting content. Confirmed by execution before
  being fixed: a chained `sync_pair_from_dest`/`sync_pair_to_dest` test on
  scenario 2's exact fixture hit exactly this conflict.
* **Rejected mid-flight, before either was committed: condition 2 as an
  either/or on `newest_source_marker(dest_tip, branch)`.** The first attempt
  at condition 2 paired "`newest_source_marker(dest_tip, branch)` already
  finds something branch-scoped on this line" with "or nothing on the line
  carries a `SourceToDest` marker at all" as an either/or. A further review,
  before either was committed, found it unsafe: `newest_source_marker` stops
  at the first branch-scoped match walking *down* from `dest_tip`, so a
  *foreign*-branded marker sitting *above* the branch-scoped one it
  eventually finds makes that scan return a stale, older boundary, while
  condition 1's own walk still grants representation, because the foreign
  marker is itself represented via case 1/case 2. The either/or then granted
  safety on the stale boundary, and the branch replayed content the foreign
  marker had already mirrored as new commits — a phantom conflict,
  CODE-001's own bug class through a new door. Reproduced by execution
  (mirror `main` partway, mirror `release` further along the same source
  line so its own marker lands above `main`'s on dest, fast-forward dest
  `main` onto dest `release`'s tip, then sync `main` again) before being
  fixed by asking the single, correct question condition 2 above states:
  not "does a branch-scoped scan find something", but "is the *newest*
  marker on the line — regardless of whose scan would find it — actually
  branded for this branch, or is there none at all". (That fix, in turn, was
  itself found range-bound incorrectly by a later review, also before
  commit — see "Corrected by a second review, also before commit" below.)
* **Also found before commit: the B1-reachability precondition's own
  refusal must not abort the whole run.** Both condition 1's walk
  (`dest_to_source_boundary`) and condition 2's walk `bail!` if B1 turns out
  not to be reachable via dest's first-parent line (decisions/0019), even
  though a prior full-ancestry `graph_descendant_of` check already passed.
  Left uncaught, that `bail!` propagated as `Err` through
  `dest_tip_represented_in_source`'s own `?`, aborting the *entire* run for
  a discovered branch instead of the per-branch halt
  [decisions/0046](0046-dest-anchors-come-from-exact-authenticated-mappings.md)
  Addendum 2 requires ("every new bound is a horizon or a per-branch halt,
  never a whole-run abort") — a configured branch already bails the
  identical way regardless (`sync_pair_to_dest_with_key`'s own `?`), so only
  the discovered-branch case actually changes behavior. Fixed by checking
  B1's first-parent reachability directly, before either walk runs, and
  returning `Ok(false)` on failure — the same per-branch shape every other
  "not accounted for" outcome in this function already has. (CODE-003: that
  precondition check, `b1_reachable_via_first_parent`, has its own
  `MAX_MARKER_SCAN_COMMITS` scan-limit `bail!` — practically unreachable
  (dest's first-parent line would need to exceed 100,000 commits without
  ever reaching B1), but it means "never a whole-run abort" isn't
  *unconditionally* true.)

## Corrected by a second review, also before commit

A second code review of this addendum, also before it was ever committed,
found condition 2's range itself wrong, with a verified one-line fix:

* **CODE-001: condition 2's range didn't match what it was guarding
  against.** As first specified above, condition 2's walk
  (`newest_source_to_dest_marker_branch_between`) searched only `dest_tip`
  down to (and including) B1 — the same range this decision's own
  represented-prefix walk (`dest_to_source_boundary`) is bounded to. But B1
  is advanced by dest→source imports, which never consult this
  source→dest-side safety gate at all — so the newest mirror marker
  genuinely on the line can sit *below* B1 once such an import has moved
  past it. When that happens, the B1-bounded search finds nothing in range
  and grants safety on an empty search, while
  [`newest_source_marker`](../../src/commands/sync/marker_scan.rs) — the
  *actual* boundary `dest_resume_point_for_branch` resumes from next, and
  which is not bounded by B1 at all — walks straight past that
  foreign-branded marker to a stale, older one further down, genuinely
  branded for this branch. The gate then grants safety on the stale
  boundary and replays already-mirrored content as a phantom conflict —
  reproduced by execution on already-mirrored content (`release.txt`),
  CODE-001's own headline symptom, hard-stopping a round-tripped branch.
  Fixed by searching the *whole* first-parent line instead of stopping at
  B1 — the one-line change is condition 2's walk stopping only at the true
  end of dest's first-parent line (bounded by `MAX_MARKER_SCAN_COMMITS`,
  never by B1), the same unbounded-by-B1 range `newest_source_marker` itself
  walks, so the two can no longer disagree about what the newest marker on
  the line is. Narrowing the walk's own acceptance criterion by searching a
  *larger* range cannot regress anything the B1-bounded version already
  refused — it can only turn some cases that were wrongly granted safety
  into correct refusals. A regression test
  (`sync_pair_to_dest_refuses_when_an_imported_dest_native_boundary_hides_a_foreign_mirror_beneath_it`,
  `tests::dest_to_source`) extends the existing
  `sync_pair_to_dest_refuses_when_a_foreign_branded_marker_sits_above_a_stale_branch_scoped_one`
  fixture with two more dest-native commits (so B1 lands above both
  existing mirror markers), a real `sync_pair_from_dest` import of a
  dest-native edit to already-mirrored content, and a second import branded
  "sibling" per this decision's own scenario-9 device; confirmed empirically
  to fail before the fix (grants safety, phantom conflict) and pass after
  (today's clean "isn't at a point this clone can safely build on" refusal).
* **CODE-004 (doc-only):** `dest_tip_represented_in_source` returns
  `Ok(false)` whenever `b1 == dest_tip`, since `graph_descendant_of` is
  non-reflexive — harmless only because `dest_tip_accounted_for`'s case 3
  already returns `ViaPriorGitprismSync` for exactly that oid and
  short-circuits before this function is ever called with that input. Noted
  in the function's own doc comment so a future direct caller doesn't
  assume otherwise.

## Corrected by a third review, after commit

An external review of the committed branch (2026-09-04) found that the
newest-mirror-marker gate above was placed on only one of the two accounting
paths, with a verified fix:

* **CODE-001: the gate ran only when `dest_tip_accounted_for` had already
  said no.** `dest_resume_point_for_branch` consulted
  `dest_tip_represented_in_source` — and therefore the
  `newest_source_to_dest_marker_branch_between` gate inside it — only as the
  second half of an `||` after `dest_tip_is_accounted_for` returned false.
  But `dest_tip_accounted_for`'s case 3 is satisfied by any ordinary
  same-branch dest→source import naming `dest_tip` exactly — the most common
  way into the shape the second-review fix guards against — and case 3 says
  nothing about which mirror marker `newest_source_marker` will find beneath
  `dest_tip`. The second review's regression test only exercised the gate
  because it branded its second import "sibling" (the scenario-9 device),
  forcing case 3 to fail. Reproduced by execution: the same fixture with n2
  imported normally for `main` produced the identical phantom conflict on
  `release.txt` the second review had fixed. Not a regression introduced by
  this decision — the same bypass existed before the addendum — but the
  addendum claimed to close exactly this symptom. Fixed by moving the gate
  out of `dest_tip_represented_in_source` into its own function
  (`newest_mirror_marker_belongs_to_branch`) that `dest_resume_point_for_branch`
  applies after *either* accounting path accepts `dest_tip`. Case 1 passes
  it trivially (`dest_tip` is itself the newest marker, branded for the
  branch); case 2 passes when nothing was ever mirrored below the graft; the
  represented-prefix path is unchanged in effect. `mirror_only_rewrite_detected`
  stays fail-safe on the new refusal: in this shape the stale branch-scoped
  boundary is still an ancestor of source's tip, so no rewrite is detected
  and the outcome is the ordinary per-branch halt.

## Consequences

* `gitprism resolve`'s source→dest path, which calls
  `dest_resume_point_for_branch` directly (`resolve.rs:308`), picks up both
  the widening and its narrowing with no change of its own — resolve and
  sync must never disagree about which dest commit is next
  ([decisions/0008](0008-ship-resolve-helper.md)).
* **Tests added:** a positive end-to-end round-trip test
  (`sync_pair_to_dest_accepts_a_dest_tip_represented_only_via_case_two`); a
  branch-scoped negative unit test on the `dest_resume_point` seam
  (`dest_resume_point_refuses_a_case_one_marker_sitting_on_an_unimported_native_commit`);
  a scenario-2 refusal regression test
  (`sync_pair_to_dest_still_refuses_a_dest_tip_parked_on_a_sibling_branchs_own_mirror`);
  a regression test for the stale-branch-scoped-boundary bug
  (`sync_pair_to_dest_refuses_when_a_foreign_branded_marker_sits_above_a_stale_branch_scoped_one`);
  a regression test for the whole-run-abort bug
  (`dest_resume_point_returns_none_instead_of_erroring_when_b1_is_off_the_first_parent_line`);
  added by a second review before commit, a regression test for CODE-001's
  range bug
  (`sync_pair_to_dest_refuses_when_an_imported_dest_native_boundary_hides_a_foreign_mirror_beneath_it`);
  and, added by a third review after commit, a regression test for the
  gate's placement
  (`sync_pair_to_dest_refuses_a_foreign_mirror_beneath_a_tip_accounted_for_by_an_ordinary_import`).
