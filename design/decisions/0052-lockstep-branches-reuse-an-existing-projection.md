---
type: Decision
title: A branch whose pending source commits are already projected by another branch reuses that projection instead of replaying a second copy; a configured branch's foreign-marker refusal becomes a per-branch halt
description: Fixes gitprism-bug-report.md (v0.1.9). Two branches advancing in lockstep each replayed the shared source commits onto their own dest tip, producing identical-tree, incomparable projections that halt every later child under decisions/0046 step 4. A branch with an existing dest ref now reuses another branch's exact authenticated projection of its newest observed-mapped pending source commit — fast-forward when its tip is on that projection's first-parent line, otherwise a two-parent adoption merge (own tip first) — but only when a bounded, authenticated lineage proof shows the reused history is exactly the projection of the branch's own pending first-parent range and the branch's own non-shared dest history is content-neutral accounting. An observed shared mapping that can't be proven, or can't be resolved, halts the branch. Amends decisions/0045: a branch refused while its dest line's newest mirror marker belongs to another branch halts per branch with a typed diagnostic, including configured branches, instead of aborting the run.
tags: [architecture, branches, markers, mapping]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-10-13T00:00:00Z }
---

# Context

`gitprism-bug-report.md` (v0.1.9, `9753a4f`). `develop` (configured) and
`feat/x` start at source `S0`; dest has `develop -> D0`, `feat/x -> M -> D0`
(`M` is feat/x's [decisions/0044](0044-a-branchs-dest-ref-always-carries-its-own-marker.md)
marker). `S1..S3` are pushed to both branches. Each branch resumes from its own
dest boundary (`dest_resume_point_for_branch`) and `build_pending_dest_tip`
replays `S1..S3` onto its own tip: `develop` gets `D1..D3`, `feat/x` gets
`E1..E3`. `D3^{tree} == E3^{tree}`; neither is an ancestor of the other.
[Decisions/0046](0046-dest-anchors-come-from-exact-authenticated-mappings.md)
canonicalization collapses `M` to `D0` but never touches `E*`, so any branch
later created from `S3` halts with `contradictory dest mappings` (0046 step 4).
0046 behaves as designed there; the defect is that the second copy is
created. Markers are not the cause: two configured branches both at `D0` fork
identically.

Recovery exposed a second defect. With dest `feat/x` force-pushed to `D3`,
`dest_resume_point_for_branch` refuses (D3 carries develop's marker). For a
configured branch [decisions/0045](0045-discovered-branch-refusals-are-per-branch-halts.md)
kept that refusal fatal, so the whole run aborts, and its message ("fetch/pull
the latest source history") does not apply.

Rejected during review with Jack (2026-10-13):

* **Comparing trees (the report's second option, or "adopt if `merge-tree`
  equals the candidate").** A `SourceToDest` marker authenticates a mapping,
  not equality with the filtered source snapshot: `build_pending_dest_tip`
  three-way-merges onto dest's tip and so preserves independent dest content.
  Reachable in the normal phase order: develop at `S_k` imports dest-native
  file `q` as source child `N` of `S_k`, then mirrors `S1..S_k` onto its
  `q`-bearing dest tip (skipping `N` by loop prevention). `S_k -> D_k` is a
  legitimate indexed mapping whose tree contains `q`; feat/x, still at `S_k`,
  never imported `q`. `merge-tree(T, D_k)` is clean and equals `D_k`, yet
  adopting it leaks `q` into feat/x with no source history, and on a configured
  feat/x the new own marker makes 0048 case 1 hide it from dest→source. 0046
  also explicitly rejects tree comparison between incomparable mappings.
* **Adoption merge with the adopted projection as first parent.** 0048's B1
  (`newest_dest_marker` for the branch) can sit on the branch's own line below
  `T`; `dest_to_source_boundary` requires B1 on dest's first-parent line
  (decisions/0019) and would abort the next dest→source run.
* **Force-moving the branch to the existing projection.** Drops `M` and any
  prior accounting commit; not fast-forward. Forbidden for configured branches
  (decisions/0038) and not a detected rewrite for mirror-only ones
  (decisions/0039).
* **Replaying unconditionally when the mapping index is truncated.** A
  truncated index can still hold an observed shared mapping for a pending
  commit; replaying then publishes exactly the second copy this decision
  prevents. Only *absence* of an entry falls back to replay (Step 1).
* **Recomputing every earlier adoption's lineage proof on each run.** Replaced
  by a writer contract on authenticated bridges (Step 3).
* **Auto-adopting a foreign-marker tip (bug 2's first suggestion).** New
  automation without a Git primitive establishing its safety; left to a
  separate decision if wanted.

# Decision

## Part 1: reuse an existing projection

Applies on the `dest_ref_exists` path of `sync_pair_to_dest_with_key` where
`dest_resume_point_for_branch` returned a boundary `B` for branch `X` with
fetched dest tip `T`, and to `resolve --direction source-to-dest` as stated in
Step 9. The brand-new-branch and mirror-only-rewrite paths (0046, 0039, 0044)
are unchanged.

**Step 0: policy first.** The decisions/0037 per-commit policy check runs over
the original `pending_commits(B, source_tip)` before anything below may shorten
the replay boundary.

**Step 1: trigger.** Let `P = pending_commits(B, source_tip)` (first-parent,
oldest first, decisions/0035). Scan `P` newest to oldest, considering only
commits replay would actually project: a commit the builder's existing
`loop_prevented(commit, key)` predicate skips (a verified `Setup` or
`DestToSource` marker) can't be duplicated, so it never triggers reuse. An
unauthenticated or otherwise invalid marker is not exempt. This filters
candidate selection only. `P` itself stays intact for the 0037 pre-pass,
Step 2, Step 5's ordering positions and its in-range import evidence. For each
considered commit with at least one observed entry in the run's mapping index,
resolve it through 0046's existing canonicalization:

* `Contradictory` (including the truncation and Finding R cases
  `resolve_for_anchor` already reports that way): per-branch halt with 0046's
  diagnostic.
* resolved to `T` or an ancestor of `T`: already represented in `X`'s dest;
  skip it and continue to older commits.
* resolved to anything else: this commit is `S_k`, its resolution is `D_k`;
  stop scanning.

If the scan finds no `S_k`, today's replay runs unchanged. That covers an
empty `P`, no observed entry, and only contained entries. If the index is
truncated (0046 addendum horizon), absence of an entry is unproven and
duplicate prevention is **not guaranteed** on this path; it is no worse than
v0.1.9.

Amended twice on 2026-10-13 during implementation, both approved by Jack.
First: the draft stopped at the newest observed entry and replayed whenever its resolution was
contained in `T`. That silently replayed older entries another branch had
projected outside `T`, a duplicate. It also deadlocked `resolve`, whose finish
check refused that older commit while `sync` kept replaying it. Second: import
markers triggered reuse even though replay never projects them. A mirror-only
branch with its own dest commits and an inherited import then halted where it
should replay and report its real conflict
(`resolve_selects_syncs_conflict_and_must_not_replay_a_sibling_branchs_dest_marker`).

Semantic boundary: an import marker alone never makes an existing branch adopt
the dest content it names. That content enters only through reuse of a later
replayable commit whose lineage includes the proven import (Step 5).
New-branch anchoring (0046) is unchanged.

Cost: no new explicit first-parent scan, but up to `|P|` exact resolutions and
containment checks, and both do ancestry work (`resolve_exact`'s
canonicalization and `graph_descendant_of`).

Otherwise the lineage proof (Steps 2–6) must pass. If it fails, the branch
halts (`Outcome::Error`, run exits non-zero, nothing pushed).

**Step 2: boundary on the first-parent line.** `P[0].parent(0) == B`. The
existing resume check accepts full ancestry; this proof needs `B` directly on
source's first-parent line below `S_k`.

**Step 3: X's own accounting chain.** Walk `T`'s first-parent line downward,
collecting the set `A`, while the current commit `C` is one of:

* an **alias**: single parent, `C^{tree} == C^1^{tree}`, a `SourceToDest`
  marker that `marker::verify` accepts for branch `X`. Add `C` to `A`; continue
  at `C^1`.
* a **bridge** (adoption merge): exactly two parents, `C^{tree} ==
  C^2^{tree}`, a `SourceToDest` marker that `marker::verify` accepts for branch
  `X`. Add `C` and `C^2` to `A`; continue at `C^1`.

The first commit that is neither, `R`, is added to `A` and ends the walk. A
commit that fails any listed condition (unauthenticated, other branch, wrong
tree, other parent count) is not trusted and becomes `R`.

*Writer contract.* The HMAC binds a marker's branch, parent list and tree
(`marker::verify`); it proves gitprism wrote that shape, not that a lineage
proof held. A bridge is trusted as a proof certificate only because of this
invariant, which every producer must keep, including future changes to
`resolve`: **an authenticated two-parent `SourceToDest` commit is written only
by Step 7, after Steps 2–6 passed for it.** Any new producer of authenticated
two-parent `SourceToDest` commits must either satisfy this proof or change the
marker format so its commits can't be mistaken for bridges.

**Step 4: shared base.** Walk `D_k`'s first-parent line downward. The first
commit in `A` is the shared base `G`; none found within budget is a proof
failure. Additionally require `G^{tree} == T^{tree}`: `G` represents exactly
the state `X` has already published. Step 1 excluded `D_k ∈ A` (every member of
`A` is `T` or an ancestor of it), so `G != D_k` and the suffix `(G, D_k]`
inspected next is never empty.

**Step 5: lineage.** Walk the suffix `(G, D_k]` upward (oldest first) with a
cursor `c`, initially `B`. Order positions are `B` < `P[0]` < … < `S_k`. Every
commit `L` in the suffix must have exactly one parent and be one of:

* a commit carrying a `SourceToDest` marker that `marker::verify_self` accepts
  (any branch), naming counterpart `s`:
  * if `L` is content-changing (`L^{tree} != L^1^{tree}`): `s` is in `P` up to
    and including `S_k`, strictly after `c`;
  * if `L` is an alias (`L^{tree} == L^1^{tree}`): `s == c`, or `s` is in `P`
    up to and including `S_k`, strictly after `c`. A `B`-naming alias is
    therefore allowed only before the first `P` counterpart; afterwards an
    alias may repeat the current counterpart but never reset to an earlier
    one;
* a commit without one, named exactly by a `DestToSource` marker that
  `marker::verify_self` accepts on a source commit in `P` up to and including
  `S_k`, whose position is strictly after `c` (decisions/0048 case 2,
  restricted to the pending range; `Setup` markers don't qualify). Amended
  2026-10-13 during implementation, approved by Jack. Without it, a branch
  catching up past another branch's import of a dest-native commit `Q` halts:
  there `S_k` is the import and `D_k` is `Q` itself;

then `c` moves to that commit's position.

`G`'s own counterpart is not inspected; Step 4's tree equality ties `G` to `T`,
and aliases directly above `G` may name only `B` or a `P` entry. A two-parent
commit (including another branch's bridge and a dest-native merge), a
dest-native commit not imported inside the range, an invalid marker, or an
out-of-range or out-of-order counterpart fails the proof. The `q` case fails
here: its import is a child of `S_k`, outside the range. Accepting dest-native
merge commits would need a separate decision.

**Step 6: budget.** Steps 3–5 share **one aggregate allowance** of
`limits::MAX_MARKER_SCAN_COMMITS` distinct commit loads per proof, using 0051's
lazy first-parent walk. Every commit those steps read counts: the accounting
walk and its tree-parent checks, the walk down `D_k`'s line, the tree
comparison, the in-range source commits scanned for imports, and each lineage
commit's parent. A commit already loaded during the same proof is reused and
not counted again. The budget is checked before each load, so a proof that
completes having used at most the allowance is valid, and needing one more load
after the allowance is spent is a horizon, i.e. a proof failure (per-branch
halt; neither adopt nor replay). `P` itself stays bounded by
`MAX_PENDING_COMMITS` as today. A commit the walk reaches but can't load is an
error, not a horizon, propagated as every existing scan does (0051);
`resolve_for_anchor` has already confirmed `D_k` is present (Finding R).

**Step 7: build and push.**

* `G == T` (fast-forward case): base is `D_k`.
* `G != T` (adoption case): base is bridge `J`, parents `(T, D_k)`, tree
  `D_k^{tree}`, a `SourceToDest` marker branded `X` naming `S_k`. `T` stays
  first parent, so B1 and every earlier `X` marker stay on `X`'s first-parent
  line.

Then `build_pending_dest_tip(boundary = S_k, dest_tip = base)` replays `P`
after `S_k`, unchanged. The pushed tip is:

* the builder's new tip, if it built at least one commit (with or without a
  later conflict);
* otherwise, in the adoption case, `J`;
* otherwise, in the fast-forward case, an alias on `D_k` branded `X` naming
  **`S_k`**, built by `build_dest_commit`. Not `branch_scoped_dest_tip`, whose
  `source_tip` counterpart could certify unapplied or conflicted suffix
  commits.

This includes a conflict at the first suffix commit: the accounting prefix (`J`
or the alias) is pushed before the conflict is reported, as sync already does
for clean prefixes. No accounting commit ever names a source commit after
`S_k` unless the builder produced it. The push is fast-forward-only (`PushMode`
unchanged).

**Step 8: same-run mappings.** After an accepted push, record in the run's
mapping index, in addition to `generated_mappings`: `S_k -> J` if a bridge was
built (even when later suffix commits sit on top of it), or `S_k -> alias` if a
fast-forward alias was built. `D_k` is already indexed. 0046 step 3 resolves
`S_k` to `D_k` in both cases (`D_k` is an ancestor of `J`; the alias
canonicalizes to `D_k`). No change to 0044 alias canonicalization is needed.

**Step 9: `resolve`.** After a sync published a Step 7 prefix, `resolve`'s own
resume scan finds `X`'s marker naming `S_k`, and its unchanged builder starts
there. `resolve` never adopts. It reconstructs the mapping index and uses the
same exact-resolution and containment helper as Step 1 at two points, so
`sync`, start and finish can't drift:

* **Start** (`start_source_to_dest`): run the Step 1–6 plan over its own `P`,
  with the same trigger filter.
  Only `Replay` proceeds. `FastForward`/`Bridge` refuses before creating the
  operation worktree and tells the operator to run `sync`, which reuses,
  publishes the prefix and reports any conflict again. `Halt` refuses with the
  actual proof or mapping diagnostic, without implying `sync` will fix it.
* **Finish** (`finish_source_to_dest`, for both `--continue` and the immediate
  finish start may perform): `--continue` never runs the start check, and
  another branch can publish a projection of `operation.source_commit` while
  the worktree is open. Before building or pushing the authenticated result,
  classify `operation.source_commit` against the authenticated operation's
  original dest base in a freshly reconstructed index:
  * no observed entry, or resolved to the dest base or an ancestor of it:
    finish proceeds as today;
  * resolved elsewhere, or `Contradictory` (including tainted or
    missing-object mappings): refuse **without deleting or modifying the
    operation worktree or its state ref**. The message tells the operator to
    save their resolution from the reported worktree, remove the stale
    operation through the existing manual flow (`git worktree remove --force
    <path>`, as the "already in progress" message does today), then run
    `sync`. Nothing is recovered automatically.

Concurrency boundary: the lookup happens immediately before the
fast-forward-only push of `X`. A projection of the same commit published to
*another* dest ref between that lookup and the push is not covered by `X`'s
lease. This is the same cross-branch boundary `sync` itself has between two
concurrent runs, and it is not closed here.

`resolve` also handles Part 2's typed refusal.

Effect on the report's sequence. `develop` processes first (equal 0046
distance, name tiebreak), producing `D1..D3`. `feat/x` sees `S3 -> D3`. `A =
{M, D0}` and `G = D0`, so the lineage `D1..D3` passes, and feat/x gets
`J = merge(M, D3)`. A child of `S3` resolves to `D3`. The next lockstep advance
`S4`: `A = {J, D3, M, D0}`, `G = D3`, lineage `{D4}`, giving `J' = merge(J, D4)`.

## Part 2: amendment to decisions/0045

`dest_resume_point_for_branch` returns a typed result. When it refuses for any
reason, including the early "not accounted for" return, it classifies the ref
with the existing whole-line walk (`newest_source_to_dest_marker_branch_between`,
extended to return the marker OID). If the newest self-verified `SourceToDest`
marker on dest's first-parent line is branded for another branch, the refusal
is `ForeignNewestMirrorMarker { branch, marker }`; otherwise it keeps its
existing reason. Classification only chooses diagnostic text: it never turns
a refusal into acceptance or replay, and never escalates it. On a refused ref:

* reaching `MAX_MARKER_SCAN_COMMITS` without finding a marker keeps the
  original refusal and its existing outcome, with a note that foreign-marker
  classification was inconclusive;
* a reached commit that can't be loaded is a genuine inspection error and
  propagates, as on every existing scan. A fetch does not by itself prove every
  ancestor object is present.

The accepted path's existing horizon semantics for this walk are unchanged.

* **Configured branch, `ForeignNewestMirrorMarker`:** per-branch
  `Outcome::Error`, `return Ok(true)`, run exits non-zero (0045's pattern).
  The diagnostic names the foreign branch and marker. It says gitprism does not
  resume a configured branch on another branch's projection and the operator
  must reconcile dest `<branch>`, e.g. by restoring it to a commit whose
  newest mirror marker is its own. It does **not** suggest deleting the ref: a
  configured branch's missing dest ref fails the earlier dest→source phase
  (decisions/0018, `sync_pair_from_dest_with_key`).
* **Discovered branch, `ForeignNewestMirrorMarker`:** the same per-branch halt
  and diagnostic, which also names deleting the dest ref as a remedy (the
  `!dest_ref_exists` path re-anchors via 0046 and writes its own 0044 marker).
* **`resolve`:** fails with the same diagnostic text.
* Every other refusal reason keeps today's behavior and message: fatal for
  configured branches, per-branch for discovered ones.

Auto-adopting a foreign-marker tip stays out of scope.

# Why

* **The mapping already exists.** 0046 made exact authenticated mappings the
  anchor source. Reusing them for an existing ref removes the duplicate instead
  of tolerating it downstream.
* **Git primitives only.** Fast-forward and a two-parent merge whose result is
  proven, not guessed. The proof uses authenticated markers already in history;
  no new durable state (0046).
* **Fail closed where a shared mapping is observed.** The proof covers the
  narrow, common shape: branches advancing together whose own extra dest
  history is only gitprism accounting. Anything else with an observed shared
  mapping halts. Only unobserved mappings in a truncated index keep today's
  behavior, and that gap is stated, not hidden.
* **Bug 2 is a policy change, not a slip.** 0045 deliberately kept the
  configured refusal fatal. Narrowing the change to one typed reason keeps
  0045's rationale (unrecognized dest content on a configured branch) for
  every other refusal.

# Consequences

* While two branches move in lockstep, each sync adds one bridge to the
  following branch. Fast-forward-only publication makes that the cost: once `T`
  contains accounting history, every update must keep it.
* **Normal-workflow restriction (accepted by the operator, 2026-10-13).** A
  branch with its own real dest commits that is later fast-forwarded on source
  onto another branch's mapped history halts. Example: a feature branch updated
  to `develop` after `develop` merged it. `G^{tree} != T^{tree}` fails the
  proof. Today this replays silently. Some of those replays later caused a
  contradiction and some didn't, but this proof can't tell which reuse would be
  safe. Merging `develop` into a feature branch that has commits `develop`
  lacks creates a real merge commit with the feature as first parent. That
  merge commit is unmapped, so it replays as today. `git merge --no-ff` turns
  the fast-forward case into this one.
* Other new per-branch halts where today replays: a boundary not on the
  first-parent line below an observed shared mapping, and a candidate lineage
  containing any two-parent commit. The latter includes another branch's own
  bridge, so even benign cross-branch reuse of a bridge-bearing line halts.
  Accepted by the operator, 2026-10-13.
* An existing ref catching up past another branch's import reuses that
  branch's projection of the shared commits around it but, as today, doesn't
  gain the imported dest-native content unless a later replayable shared commit
  is reused on top of it (Step 1's semantic boundary). The test
  `loop_prevented_skips_an_inherited_cross_branch_marker_…` keeps its original
  expectations. It now passes through reuse of the original `s1` projection
  rather than through the builder skipping `M`, so the skip rule itself is
  covered by `build_pending_dest_tip_skips_another_branchs_import_marker`.
* In a truncated index, an unobserved shared mapping still gets a duplicate
  (Step 1). Accepted by the operator, 2026-10-13.
* Existing contradictory mappings (already published) are not repaired; they
  stay operator-resolved under 0046 step 4. Part 2 makes the recovery attempt
  halt cleanly instead of aborting the run.
* `resolve` changes: it checks for observed shared mappings at start and again
  before finish (Step 9), and handles Part 2's typed refusal. Its builder is
  unchanged. A projection published to another dest ref between finish's
  lookup and its push can still produce a duplicate (Step 9's concurrency
  boundary).
* The writer contract in Step 3 binds every future producer of authenticated
  two-parent `SourceToDest` commits.

# Tests

Only six tests were written first and seen failing: the report sequence in
one run, in separate runs and without a marker (1), two successive advances
(2), the `q` case (4) and the configured foreign-marker halt (14). `design/log.md`'s 0052 entry lists the tests written
after the implementation.

1. Report sequence, same run and separate runs: lockstep `S1` on both branches,
   then a child from `S1`. Expect one canonical projection and no
   contradiction. Both the marker shape (`feat/x -> M -> D0`, bridge) and the
   no-marker shape (both configured at `D0`, fast-forward).
2. Two successive lockstep advances; then reconstruction in a fresh clone
   resolves both `S_k` to develop's projections.
3. Configured `feat/x` with B1 on its retained line, then a real dest-native
   edit imported by dest→source after a bridge: no abort.
4. Candidate lineage contains unrepresented `q` (Jack's sequence): halt,
   nothing pushed.
5. `.gitprismignore` policy mismatch inside the skipped `B..S_k` range: 0037
   halt.
6. Same-run visibility: a branch processed later in the run sees `S_k -> J`
   (and the fast-forward alias) and resolves to `D_k`.
7. Budget: a proof completing at exactly the allowance passes; one needing one
   more load halts (neither adopts nor replays).
8. Truncated index: an observed entry in `P` halts; no observed entry replays.
9. Bridge trust: unauthenticated, wrong-branch, wrong-tree and
   wrong-parent-count bridges on `T`'s line aren't trusted (adoption halts).
10. Lineage order: a `B`-naming alias after an in-range counterpart fails; a
    repeat of the current counterpart passes. Imports: an exact in-range
    import passes; out-of-range, unauthenticated, `Setup`-marked and
    out-of-order imports and a two-parent dest-native commit fail.
11. Adoption, then a conflict at the first suffix commit, in both bridge and
    fast-forward shapes: the prefix (`J`, or the alias naming `S_k`) is pushed,
    the conflict is reported, no marker names a commit after `S_k`, and
    `resolve` continues from `S_k`.
12. `resolve` start: a reusable shared mapping in its pending range refuses
    before creating a worktree and points at `sync`; a proof failure reports
    its own diagnostic; all observed mappings contained in `T` lets it start.
12a. Step 1 scan: newest observed mapping contained in `T` but an older one not
    contained → no duplicate push, proof failure halts; newest contained plus
    an older `Contradictory` → halt; all contained → replay. Import-only
    pending mappings → replay; an import plus an older noncontained
    replayable mapping → the older one is selected; `D1 → Q → D2` with `N` in
    range → reuse accepted. The existing resolve conflict test passes
    unchanged.
12b. `resolve` finish: a mapping contained in the operation's dest base
    proceeds; a noncontained or `Contradictory` one refuses and keeps all
    operator state. Includes the conflict-prefix/continuation scenario, so the
    resolve deadlock can't return.
13. `resolve` start (no shared mapping) → another branch publishes a projection
    of `operation.source_commit` → `--continue`: refuses, pushes nothing,
    worktree and state ref retained.
14. Configured branch with a foreign-marker tip (the report's exact recovery
    state, refused by the early accounting check): that branch halts with the
    typed diagnostic, other branches sync, run exits non-zero. Discovered
    variant names ref deletion.
15. Early-refused discovered ref with no mirror marker within the
    classification horizon: that branch halts with its original refusal (plus
    the inconclusive note), a later branch is still processed, run exits
    non-zero.
16. Configured branch with a deleted dest ref still fails as decisions/0018
    specifies.
