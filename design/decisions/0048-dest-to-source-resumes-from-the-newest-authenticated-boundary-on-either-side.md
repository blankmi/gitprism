---
type: Decision
title: dest-to-source resumes from the newest authenticated boundary on either side
description: Fixes CODE-001 (docs/2026-09-02_REPOSITORY_REVIEW.md, section 3) — a branch cut from a round-tripped branch after `setup`, later added to `config.branches`, only inherited the parent's `Setup` graft, so `pending_dest_commits` replayed the parent's own mirrored commits and hard-stopped on a phantom conflict, with no supported path since `setup` refuses a branch that already carries an inherited marker. `pending_dest_commits`'s boundary is now the newest commit on dest's first-parent line that is either the existing `Setup`/`DestToSource` marker boundary or a self-verified `SourceToDest` marker whose source counterpart is an ancestor of source's current tip — computed only after the existing boundary is confirmed to still be on dest's line, so a force-rewound dest tip is refused exactly as before rather than silently accepted.
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

# Decision

The dest→source boundary for branch `B` is the **newest commit on dest `B`'s
first-parent line** that is either:

1. **B1** — the dest commit named by the newest `Setup`/`DestToSource` marker
   on source `B`'s first-parent line: today's boundary, `newest_dest_marker`'s
   unchanged result.
2. **B2** — a `SourceToDest` marker commit, self-verified against its own
   recorded branch (`marker::verify_self`, not `B`'s name), whose source
   counterpart is an ancestor of (or equal to) source `B`'s current tip
   (`repo.graph_descendant_of`).

B1 remains a precondition, exactly as `pending_dest_commits` enforces today:
if B1 is missing from the local object database, or
`graph_descendant_of(dest_tip, B1)` is false (and `B1 != dest_tip`), refuse
with today's "isn't an ancestor of dest's current tip" message before any
further walk. Only once B1 is confirmed does a second, bounded walk of dest's
first-parent line run — from `dest_tip` down to B1 inclusive, newest first,
stopping at the first commit satisfying B2. If none does, the boundary stays
B1. This second walk is bounded both by reaching B1 and by
`MAX_MARKER_SCAN_COMMITS`, the same static limit every other marker scan in
this codebase uses ([decisions/0032](0032-bounded-repository-controlled-data.md)).

The precondition on B1 is what keeps this fail-closed rather than opening a
new hole. Without it, a dest branch force-rewound to an older commit that is
itself a valid `SourceToDest` marker whose counterpart is an ancestor of
source's tip would be accepted as B2 before the walk could ever reach the
newer, already-imported commit B1 names — silently reintroducing the exact
divergence today's ancestor check exists to catch. B1's precondition is
checked first, unconditionally, so this can't happen: a dest tip that fails
it is refused before B2 is ever evaluated.

Loop prevention inside `pending_dest_commits` — filtering out a pending
commit that itself carries a `SourceToDest` marker for `branch` — stays
scoped to `branch`'s own name, unchanged. This decision only widens the
*boundary* B2 can name; it does not change which pending commits are dropped
once the boundary is found. Widening loop prevention itself to
`verify_self` (accepting any branch's `SourceToDest` marker, not just `B`'s
own) would be a different, wrong fix: a customer fast-forwarding dest
`release` into dest `main` must still import `release`'s independent content
into source `main`, and that content arrives on `main`'s first-parent line
carrying `release`'s own `Gitprism-Branch` marker — `verify_self` loop
prevention would recognize that marker and silently drop the content instead
of reflecting it back.

# Why

* **Dest's first-parent line is the only place both candidate boundaries are
  directly comparable.** B1 and B2 are each expressed on a different side —
  B1 is discovered on *source*'s history and only then checked against dest;
  B2 lives natively on *dest*'s history. Neither side's own history orders
  them against each other; only a single revwalk of dest's first-parent line,
  newest-to-oldest, can ask "which of these did I reach first."
* **`verify_self` plus the ancestor check is what makes trusting an inherited
  marker safe.** A `SourceToDest` marker commit on the new branch's dest
  history was written by source→dest for whatever branch it was mirroring at
  the time (`main`, not the new branch) — `marker::verify` against the new
  branch's own name would reject it outright, by design (decisions/0003's
  per-branch scoping). `verify_self` (decisions/0046, its addendum
  introducing exactly this self-verification shape for an inherited marker)
  authenticates it against its *own* recorded branch instead, so the HMAC
  still proves it's a genuine gitprism commit, just not necessarily this
  branch's own. That alone isn't sufficient — an inherited marker only means
  "source, at some point, definitely knew about the source commit this dest
  commit was built from," not "source knows about it *now*." The
  `graph_descendant_of(source_tip, counterpart)` check supplies the missing
  half: source's current tip must still descend from that source commit, so
  resuming from this dest commit can never skip content source no longer has
  reachable, or has since diverged from.
* **A qualifying B2 can never sit above an unimported dest-native commit.**
  `dest_tip_accounted_for` (`src/commands/sync/anchor.rs:62-102`) only lets
  source→dest push a mirror commit onto a dest tip gitprism already
  recognizes: the tip is itself a marker (case 1), the tip is exactly the
  setup graft (case 2), or source's own history already carries a
  `Gitprism-Dest-Commit` trailer naming that tip exactly, i.e. dest→source
  already imported it (case 3). So any dest-native commit sitting on `B`'s
  first-parent line below a `SourceToDest` marker commit was necessarily
  already imported into source before that marker commit was ever pushed.
  When a later B2 candidate's `graph_descendant_of(source_tip, counterpart)`
  check passes against the *current* source tip, that import is still
  reachable from it, so resuming from B2 carries the same guarantee forward:
  no dest-native commit between B1 and B2 was skipped, because it was
  already in source before B2 existed. Scenario 5 (`C0` beneath `D(s2)`)
  shows the converse — when the ancestor check fails, the walk correctly
  keeps going past the disqualified B2 candidate instead of trusting it, so
  `C0` is still picked up. This is also why the B1 precondition alone isn't
  sufficient: B2's own soundness depends on this chain, not merely on being
  newer than B1.
* **`graph_descendant_of` is Git's own primitive for exactly this
  question**, already the mechanism `pending_dest_commits` uses for B1's own
  precondition and that every other ancestor check in this codebase
  ([decisions/0028](0028-operation-lock-and-local-advance-cas.md)'s
  local-advance CAS guard, [decisions/0045](0045-discovered-branch-refusals-are-per-branch-halts.md)'s
  "point this clone can safely build on" check,
  [decisions/0039](0039-mirror-only-source-rewrites-rebuild-the-projection.md)'s
  rewrite detection) reaches for first, per AGENTS.md's "ask whether Git
  already provides a safe primitive" rule. No new mechanism is introduced —
  B2 reuses the same primitive B1 already depends on, applied to a different
  pair of commits.
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

* One additional bounded revwalk of dest's first-parent line per configured
  branch, run only after B1's precondition already succeeds. The added walk
  is bounded by O(dest_tip..B1) — the same range `pending_commits`'s own
  revwalk already traverses once the boundary is found — so no new
  asymptotic cost is introduced. For an already round-tripped branch the
  boundary typically does move from B1 to a later B2 commit (dest's tip is
  usually gitprism's own `SourceToDest` commit, whose counterpart is
  source's own tip, so B2 is found immediately) — the resulting
  pending-commit set is unchanged regardless, because branch-scoped loop
  prevention already excludes those commits whichever boundary is chosen.
* `setup`'s refusal of a branch whose local tip already carries an inherited
  `Setup`/`DestToSource` marker (`src/commands/setup.rs:229-241`) is
  unchanged — `setup` remains a one-time step (decisions/0006, 0012) — but it
  now has a real supported path on the other side: `sync` correctly resumes
  such a branch instead of mis-resuming it, so promoting a mirrored branch
  into `config.branches` after the fact is no longer a dead end.
* The documented limitation from
  [decisions/0019](0019-marker-scans-are-first-parent-only.md) (marker scans
  assume the tracked branch stays first-parent of its own merges) applies to
  this new dest-side walk exactly as it already applies to
  `newest_dest_marker`'s source-side walk and `newest_source_marker`'s
  dest-side walk — a B2 candidate merged into `B` other than as first parent
  is not reachable by this scan, same accepted tradeoff, no new exposure.
* `pending_dest_commits`'s doc comment, and `newest_dest_marker`/
  `scan_for_dest_marker`'s, need updating to describe the boundary as B1 *or*
  a qualifying B2, not B1 alone (step 4).
* `gitprism resolve`'s dest→source path, which calls `pending_dest_commits`
  directly (`resolve.rs:1171`, `:1258`), picks up the fix with no change of
  its own — resolve and sync must never disagree about which dest commit is
  next ([decisions/0008](0008-ship-resolve-helper.md)'s original invariant).
