---
type: Decision
title: Mirror-only force push is a compare-and-swap lease against the fetched dest tip
description: PushMode::ForceMirrorOnly's outright `+<oid>:refs/heads/<branch>` refspec is replaced with an explicit `--force-with-lease=refs/heads/<branch>:<fetched-dest-oid>`, so a concurrent dest advance between fetch and push is rejected instead of silently overwritten. Revises decisions/0038's blanket "no lease mechanism anywhere" conclusion for mirror-only force only — a lease must still never enable force on a round-tripped branch. `PushMode::ForceMirrorOnly` gains a required `expected_dest: Oid` field.
tags: [architecture, push, concurrency, branches]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-21T00:00:00Z }
---

# Context

[decisions/0038](0038-branch-authority-determines-whether-history-may-be-rewritten.md)
(as amended in `119c16b`) permits force-updating a mirror-only dest branch,
and [decisions/0039](0039-mirror-only-source-rewrites-rebuild-the-projection.md)
defines exactly when: a positively detected source-side rewrite, checked via
four direct conditions before any push is attempted.

**The defect.** `PushMode::ForceMirrorOnly` (`src/git.rs`'s `push_refspec`)
currently builds `+<commit>:refs/heads/<branch>` — an unconditional force,
sent via `push` (`src/git.rs`). The sequence in
`sync_pair_to_dest_with_key` (`src/commands/sync/mod.rs`, since split) is: fetch dest's tip `D`
(`sync/mod.rs:353-361`), positively detect the mirror-only rewrite via
`mirror_only_rewrite_detected` (`sync/mod.rs:387-389` at the time, defined at
`sync/anchor.rs:1214` now), build replacement projection `N` from the graft-derived
`(boundary, dest_tip)` (`sync/mod.rs:401-409`), and push `N`
(`sync/mod.rs:555`). If another writer advances dest from `D` to `D2` between
the fetch and the push, the `+`-prefixed refspec replaces `D2` with `N`
outright and git reports success — no rejection is produced. Decisions/0009's
refetch-and-recompute retry arm (`sync/mod.rs:557-570`,
`git::PushOutcome::RejectedNotFastForward if attempt < MAX_RACE_RETRIES`) is
therefore unreachable for this specific race, which makes decisions/0039's
own Decision-section flow diagram ("if dest moves during the operation,
refetch/recompute") a promise the code cannot currently keep for
`ForceMirrorOnly`.

**Empirical facts, verified against real git.** Probed with
`git push --porcelain` against a local bare remote, with the remote already
advanced from `D1` to `D2` while the local side held a non-descendant commit
`N`:

* A stale explicit lease (`--force-with-lease=refs/heads/main:<D1>`) is
  rejected, exit `1`, printing the porcelain line:
  `!\t<oid>:refs/heads/main\t[rejected] (stale info)`. The remote is left at
  `D2`, unmodified.
* A correct explicit lease (`--force-with-lease=refs/heads/main:<D2>`)
  succeeds, exit `0`, printing
  `+\t<oid>:refs/heads/main\t<D2>...<N> (forced update)`.
* A plain non-fast-forward push (no lease at all) prints
  `[rejected] (fetch first)`.

Critically: `is_non_fast_forward_rejection` (`src/git.rs:197-204`) matches a
porcelain line whose status field is `!` and whose last tab-separated field
starts with `[rejected]`. A stale-info lease rejection satisfies both fields
exactly the same way a plain non-fast-forward rejection does. **The existing
retry loop therefore already routes a stale lease into decisions/0009's
refetch-and-recompute with no new plumbing** — `push` (`src/git.rs:525-572`)
needs no new outcome variant, and `sync_pair_to_dest`'s retry arm needs no
new match case.

# Decision

An authorized mirror-only force push is performed as an explicit
compare-and-swap lease tied to the dest OID just fetched:

```
git push \
  --force-with-lease=refs/heads/<branch>:<fetched-dest-oid> \
  <dest-url> \
  <new-tip>:refs/heads/<branch>
```

Flow, replacing today's unconditional-force sequence:

1. Fetch dest tip `D`.
2. Confirm the mirror-only rewrite (decisions/0039's four conditions) and
   build replacement `N`.
3. Push `N` only if dest still equals `D` — the lease's job.
4. If the lease fails (stale info), refetch, re-evaluate branch authority
   and rewrite detection from scratch, rebuild `N` again, and retry within
   decisions/0009's existing bound (`MAX_RACE_RETRIES`, `sync/mod.rs:80` at the time) — the
   same loop `sync_pair_to_dest` already runs, since a stale lease is
   classified identically to any other non-fast-forward rejection.
5. After retries are exhausted, stop for operator intervention
   (`divergence_after_exhausted_retries_message`, `sync/mod.rs:90` at the time) — never fall
   back to an unconditional force.

**The lease does not decide whether forcing is authorized.**
`config.branches` membership (decisions/0017's mirror-only/round-tripped
distinction) and decisions/0039's rewrite detection do that, both
unchanged by this decision. The lease only prevents a concurrent dest
update, landing in the fetch-to-push window, from being overwritten unseen.
These are two separate mechanisms and must not be conflated — that
conflation is exactly what produced the error this decision revises 0038
for (see Why).

**Explicit `<ref>:<expect>` form only, never bare `--force-with-lease`.**
gitprism pushes by URL, not via a configured remote with a maintained
remote-tracking ref, so the implicit form's remote-tracking-ref heuristics
have nothing correct to compare against. The explicit form is git's own
documented, stable mechanism for exactly this shape
(<https://git-scm.com/docs/git-push>, `--force-with-lease=<refname>:<expect>`).

**Round-tripped branches are entirely unaffected.** They keep a plain push
with no `--force`, no lease, and no `+`-prefixed refspec —
`PushMode::FastForwardOnly` is unchanged. Git's own default non-fast-forward
refusal remains their sole protection. A lease must never be what enables a
force on a round-tripped branch; nothing in this decision grants one.

**Required API shape**, so the unconditional variant becomes
unconstructible:

```rust
PushMode::ForceMirrorOnly { expected_dest: Oid }
```

replacing today's unit-like `ForceMirrorOnly` (`src/git.rs:500-504`).
`push_refspec` is unaffected in shape (still returns the `<new-tip>:<ref>`
side of the command); `push` (`src/git.rs:525`) builds the
`--force-with-lease=<ref>:<expected_dest>` argument from the new field
whenever `mode` is `ForceMirrorOnly`, instead of a `+`-prefixed refspec.

# Why

**0038's rejection was evaluated against the wrong question.** 0038 checked
whether a lease could substitute for git's fast-forward-only default — it
verified directly that a `--force-with-lease` with a *correct* expected
value still force-pushed a divergent history and reported `(forced
update)`, which is true and is exactly why a lease stays banned as a
mechanism for round-tripped branches (0038's authority table, unedited by
this decision). But 0038 never evaluated the lease as compare-and-swap
concurrency control wrapped around a force **already authorized by other
means**. The concurrent dest advance a lease catches (`D` → `D2` in the
fetch-to-push window) is not part of the authorized rewrite at all — it is
a second writer's unrelated, legitimate advance that gitprism's own rewrite
detection never examined and has no basis to discard. 0038's mirror-only
authority argument establishes that source is authoritative for *the
branch's history as rewritten*; it says nothing about a dest advance that
happens *during* gitprism's own fetch-build-push window, which is precisely
the race decisions/0009 already treats as worth catching for
`FastForwardOnly` pushes. 0038's specific claim that a lease "adds no
safety a deliberate, authorized mirror rewrite needs" is what the empirical
facts above falsify: a lease adds exactly the safety of not silently
discarding `D2`, which an unconditional `+` refspec cannot provide and a
plain non-force push cannot even attempt once force is authorized.

**Why compare-and-swap, not another `git ls-remote` check.** A pre-push
`git ls-remote` confirmation was considered and rejected: it is non-atomic
— dest can still move between the `ls-remote` and the `push` — and would
leave open exactly the same window this decision closes. `--force-with-lease`
performs the compare-and-swap atomically as part of the push itself, on the
server side of the ref update.

# Consequences

* **The payload is load-bearing: `expected_dest` must be the dest tip as
  actually fetched.** In `sync_pair_to_dest`'s `match
  dest_resume_point_for_branch(...)` (`sync/mod.rs:375-414` at the time; `dest_resume_point_for_branch` itself now lives in `sync/anchor.rs`), the rewrite arm
  (`sync/mod.rs:401-409`) rebinds the outer `dest_tip` binding to
  `rebuild_dest_tip` — the graft-derived rebuild base
  `newest_dest_marker_opt_for_branch` returns — not to the value fetched at
  `sync/mod.rs:356-361`. The implementation must capture the fetched dest OID
  into its own variable *before* that `match` rebinds `dest_tip`, and pass
  that captured value as `expected_dest`. Using the post-match value would
  make every lease compare against `rebuild_dest_tip` instead of dest's
  actual current ref, so the lease would always be stale and
  `ForceMirrorOnly` would never succeed.
* **`RejectedNotFastForward` and `is_non_fast_forward_rejection` become
  slightly narrower names than what they now classify.** A stale-info lease
  rejection is not literally a non-fast-forward rejection (the porcelain
  reason is `(stale info)`, not `(fetch first)`), but it is matched by the
  same `!` + `[rejected]` predicate and handled by the same retry path.
  Whether to rename either identifier for clarity is left to the
  implementation commit; this decision does not require it.
* **decisions/0009's mechanism is unchanged.** Refetch-and-recompute,
  the bounded retry count, and the porcelain-based rejection classification
  all stand exactly as written; this decision only makes `ForceMirrorOnly`
  capable of producing the rejection decisions/0009 already knows how to
  handle.
* **decisions/0039's detection and rebuild logic are unchanged.** The four
  conditions, the graft-derived rebuild base, and the "force is not an
  escalation after failed retries" rule all stand; this decision only
  changes how the resulting push is sent to git.
* **decisions/0038's blanket "no lease mechanism anywhere" conclusion is
  revised, for mirror-only force only.** Round-tripped branches keep zero
  lease exposure, per the unedited authority table; 0038's own file is
  annotated in place (not rewritten) to point here.
* **Satisfies AGENTS.md's "prefer an established Git primitive over novel
  automation."** `--force-with-lease=<ref>:<expect>` is git's own documented
  compare-and-swap primitive for exactly this hazard, not a bespoke
  mechanism gitprism invents; the alternative considered and rejected
  (a `git ls-remote` pre-check) would have been the novel, non-atomic
  automation the project defaults away from.
* **Guard tests are refined, not loosened.** decisions/0038's role-aware
  guard ("round-tripped pushes never emit `--force`, `--force-with-lease`,
  or a `+`-prefixed refspec") is unchanged; its mirror-only assertion
  changes from "may use `PushMode::ForceMirrorOnly`" (a bare force) to "uses
  `PushMode::ForceMirrorOnly { expected_dest }`, emitting
  `--force-with-lease=<ref>:<expected_dest>` and never a `+`-prefixed
  refspec." A new test must cover the stale-lease case directly: dest
  advances between fetch and the rewrite-detection push; the lease is
  rejected; refetch-and-recompute (or a re-run of decisions/0039's
  detection) resolves it within the retry bound rather than clobbering the
  concurrent advance.
